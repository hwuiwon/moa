# MOA Simplification / Over-Engineering Audit

_Generated 2026-07-03 by a multi-agent audit workflow (18 subsystem/cross-cutting finder agents, each candidate finding then handed to an independent adversarial verifier that tried to refute it against the real code)._

_Updated 2026-07-03 by a follow-up multi-agent verifier pass over every formerly unverified finding. No unverified findings remain._

## How this was produced & how to read it

- **Scope:** the whole `crates/` workspace plus `scripts/`, `Makefile`, and compose files, read as-is on disk. Findings are things that look **more complex than the problem requires**, where a concrete simpler design exists. Complexity mandated by `AGENTS.md`, Restate replay/durability, `docs/18-performance.md`, or `docs/08-security.md` was excluded by construction.
- **Pre-production assumption baked in:** MOA has no external users and no backwards-compatibility requirement, so "kept only for compatibility" counts as over-engineering.
- **Verification tiers:** the original audit ran out of verifier budget partway through; the follow-up pass classified the remaining raw claims against current on-disk code.
  - ✅ **Confirmed** — an independent skeptic read the code and failed to refute the claim.
  - 🟡 **Adjusted** — skeptic found the core claim right but corrected the proposal or side effects (see the revised note).
  - ❌ **Refuted** — skeptic found the complexity is load-bearing or the claim is factually wrong. Listed so you know it was considered and rejected.
- Every finding cites exact files/line ranges. The finder graph can be slightly stale, so confirm line numbers before editing.
- **Do not batch-apply.** Another agent is editing this repo concurrently; each fix should be re-checked against current `main` first.

## Tally

| Tier | Count |
|---|---|
| ✅ Confirmed | 46 |
| 🟡 Adjusted (confirmed with corrections) | 52 |
| ❌ Refuted (rejected, do not act) | 3 |
| **Total raw findings** | **101** |

## Implementation Priority Groups

_Added 2026-07-03 from a current-code subagent prioritization pass. These groups are an execution order, not a change to the original audit verdicts._

### Group A: Small, Low-Risk Deletes

Start here. These remove dead state, redundant aliases, or single-use abstractions with narrow blast radius.

| Priority | Finding | Status | Notes |
|---|---:|---|---|
| A1 | #4 | done | Removed dead turn classifier workflow flag and the test-only worker helper. |
| A2 | #1 | done | Removed pass-through progress cadence/request plumbing. |
| A3 | #3 | done | Replaced fuzzy BrainResponse lineage lookup with exact sequence lookup. |
| A4 | #60 | done | Deleted the dead `memory.auto_bootstrap` field and `MOA_MEMORY_AUTO_BOOTSTRAP` overlay. |
| A5 | #82 | done | Deleted no-op `xtask migrate-test-db` and four CI probe steps. |
| A6 | #83 | done | Removed the dead `inspectable_files` root-probing branch and folded the loadtest config wrapper. |
| A7 | #8 | done | Deleted the unwired `LlmChatClient` fallback chain while keeping single-client retry behavior. |
| A8 | #29 | done | Removed the redundant Gemini Flash-Lite preview catalog alias and exact-entry tests. |
| A9 | #17 | done | Deleted unused `SigningKeyVault`/`LocalSigningKeyVault` surface and its `async-trait` dependency. |
| A10 | #18 | done | Removed the reference-only `ct-merkle` root, re-export, smoke test, dependency, and stale docs. |
| A11 | #88 | done | Collapsed single-impl `DeliverySink` into an inherent `ProviderDeliverySink::deliver` method. |

### Group B: Local Mechanical API Shrinks

Do these after the small deletes or when already touching the owning crate.

- #6 env-based provider registry path. **done:** deleted dead `ProviderRegistry::from_env` and descriptor env-factory fields.
- #7 Gemini embedding role/config surface.
- #9 `ProviderSelection` string round-trip. **done:** removed the wrapper and kept the provider/model selection typed.
- #14 hands normalization fallback duplication. **done:** descriptor-less fallbacks are generic, and constant command preview metadata is collapsed.
- #15 dead session constructors/blob enum surface. **done:** removed unused constructors and the unimplemented `object_store` blob backend.
- #38 duplicate analytics read paths. **done:** collapsed tenant/cache analytics reads to the unsuffixed control-plane-backed functions and replaced local enum parsers with `queries::from_db`.
- #42 `ProcedureCondition::Expression`. **done:** removed the unsupported expression condition and schema branch; kept live `EdgeNotFound`.
- #45 unused `CredentialVault` methods except live `set`. **done:** removed dead `delete`/`list` from the vault trait and impls.
- #58 `MemoryScope::ancestors()` identity chain. **done:** deleted the identity helper, `PlannedQuery::scope_ancestors`, and duplicate cache-key `layers=`.
- #59 `PiiSpan.replacement` compatibility field. **done:** removed the per-span replacement override and slow-path identity helper.
- #65 embedding provider alias methods. **done:** kept only `model_id()` and `dimensions()` on `EmbeddingProvider`.
- #73 eval reporter subsystem. **done:** deleted the unused reporter trait/module/reexports and self-test.
- #74 unwired pairwise LLM judge. **kept:** user chose to retain pairwise judging; added an offline pairwise eval set that exercises swapped-order candidate win, baseline win, and no-agreement cases.

### Group C: Medium Refactors

Batch these with focused crate checks and less frequent live/perf gates because they touch hotter or broader code paths.

- #2 `turn_progress` state-key consolidation.
- #5 test-only default brain pipeline builder.
- #11 weak default methods on `KnowledgeRepository`.
- #12 knowledge dead code/config knobs.
- #13/#85 single-impl `IngestionObserver`.
- #20 `LearningReviewStore` boxed-future cleanup.
- #21 skills regression report/file writer.
- #22 dead messaging modules.
- #23 analytics wire/domain DTO twins.
- #27 retrieval cache TTL/version config.
- #28 embedding builder duplication.
- #33 dead `ToolRouter` execute wrappers.
- #48 endpoint function-pointer registry.
- #49 session-store inner delegates.
- #50 mandatory Redis feature cleanup.
- #76 eval-core dead API. **done:** removed dead discovery helpers/reexports, unused error variants, unused single-case provider injection method, and unsupported live long-conversation mode.

### Group D: Product-Scope Or High-Risk Cleanup

Keep these out of opportunistic small-delete batches. They need one explicit product/runtime decision or broader validation gate.

- #10/#30 parser webhook and vendor adapter deletion should be planned together.
- #31/#92 Daytona/E2B needs one runtime/build story before deletion or ungating.
- #32 stdio MCP transport conflicts with local-dev docs and needs a product decision.
- #37 migration source consolidation requires accepting a DB wipe/refinery-history simplification.
- #52 dual compaction owners changes context pipeline behavior and needs eval/performance baselines.
- #55 Turbopuffer/outbox/promotion removal is a product architecture decision.
- #61 env overlay rewrite should follow smaller env-surface deletions.
- #67 single-variant action rule scope is valid but wide.
- #89 experiments/internal-eval feature cleanup needs delete-vs-always-compile direction.

## Index

| # | Tier | Area | Finding |
|---|---|---|---|
| 1 | ✅ | Orchestrator — workflows & turn_driver | Progress-emit plumbing threads two dead parameters and a cadence struct through every call site |
| 2 | ✅ | Orchestrator — workflows & turn_driver | turn_progress duplicates state loaders and journaled clocks per context type and spreads one struct over four Restate keys |
| 3 | ✅ | Orchestrator — workflows & turn_driver | BrainResponse lineage record re-fetched by fuzzy text match instead of by its known sequence number |
| 4 | ✅ | Orchestrator — workflows & turn_driver | Dead classifier input is_workflow_context and test-only production helper |
| 5 | ✅ | moa-brain (context pipeline) | Two parallel pipeline assemblies: test-only build_default_pipeline duplicates the production stage list in the public API |
| 6 | ✅ | moa-providers | Dead parallel env-based provider construction path in registry/descriptors |
| 7 | ✅ | moa-providers | Unused Gemini EmbedRole variants driven by a never-set config knob |
| 8 | ✅ | moa-providers | Speculative unwired fallback chain inside LlmChatClient |
| 9 | ✅ | moa-providers | ProviderSelection stringly-typed round-trip over an existing typed API |
| 10 | ✅ | moa-knowledge | Parser-webhook stack (routes, dual signature verification, config knobs) for an async parse flow that does not exist |
| 11 | ✅ | moa-knowledge | KnowledgeRepository default method bodies silently replace concurrency-critical semantics with weaker in-memory versions |
| 12 | ✅ | moa-knowledge | Dead code and dead config knobs left behind by refactors |
| 13 | ✅ | moa-knowledge | IngestionObserver trait has exactly one implementation workspace-wide and exists only to add a fifth generic parameter to the pipeline |
| 14 | ✅ | moa-hands (tools/sandboxes/MCP) | normalization.rs keeps a shape-based fallback path that duplicates the descriptor-driven review/pattern logic, including dead tool-name string matches |
| 15 | ✅ | session / db / migrations / runtime-store | Dead constructor and config surface on PostgresSessionStore and the blob backend enum |
| 16 | ✅ | lineage / ocsf / observability | Citation 'NLI cascade' is speculative machinery around a model that does not exist |
| 17 | ✅ | lineage / ocsf / observability | SigningKeyVault trait and LocalSigningKeyVault have zero consumers |
| 18 | ✅ | lineage / ocsf / observability | ct-merkle dependency exists only to keep an unused 'reference shape' function visible |
| 19 | ✅ | skills / artifacts | Dead session-only skill distillation/improvement lane kept parallel to the experience-native lane |
| 20 | ✅ | skills / artifacts | LearningReviewStore hand-rolls boxed-future trait plumbing and carries a never-called method; moka/async-trait deps unused |
| 21 | ✅ | skills / artifacts | Regression module ships report/decision types and a file-writing suite generator that production never uses |
| 22 | ✅ | edge / messaging / security | control.rs and edit_window.rs are dead modules with test-only consumers |
| 23 | ✅ | edge / messaging / security | Analytics wire response structs duplicate domain DTOs field-for-field, with hand-written copy mappers at the edge |
| 24 | 🟡 | Orchestrator — workflows & turn_driver | ConsolidateDurableSteps trait layer whose only purpose is a tautological replay test |
| 25 | 🟡 | Orchestrator — workflows & turn_driver | Fully parallel Review vs Signal pipelines in ProcedureExecution |
| 26 | 🟡 | moa-brain (context pipeline) | Hand-rolled NER returns rich labeled spans that no consumer reads; production planner ships a 12-line MOA-dev gazetteer |
| 27 | 🟡 | moa-brain (context pipeline) | Retrieval cache carries a dead per-tenant version-TTL cache and a config struct never customized anywhere |
| 28 | 🟡 | moa-providers | Near-duplicate build_semantic / build_vector embedding builder paths |
| 29 | 🟡 | moa-providers | Backwards-compat catalog entry gemini-3.1-flash-lite-preview is fully redundant |
| 30 | 🟡 | moa-knowledge | Five external vendor adapters pre-production: Merge provider and Unstructured/Reducto parsers are parallel paths to the one live stack (Nango + native + LlamaParse) |
| 31 | 🟡 | moa-hands (tools/sandboxes/MCP) | Daytona and E2B cloud sandbox adapters are dead code behind feature flags no consumer enables |
| 32 | 🟡 | moa-hands (tools/sandboxes/MCP) | Stdio MCP transport (~450 LOC incl. concurrent demux machinery and a background reader task) is unreachable from any production path |
| 33 | 🟡 | moa-hands (tools/sandboxes/MCP) | Three of five public ToolRouter execute entry points (plus eager install_files) have no production callers and duplicate ~50 lines of span/policy boilerplate each |
| 34 | 🟡 | moa-hands (tools/sandboxes/MCP) | recovery.rs duplicates the entire retry/reprovision state machine for hand vs MCP execution, including a 12-line counter-update block copy-pasted four times |
| 35 | 🟡 | moa-hands (tools/sandboxes/MCP) | ToolRouter carries a concrete LocalHandProvider side-channel next to the dyn HandProvider map, special-cased by provider-name string comparison |
| 36 | 🟡 | session / db / migrations / runtime-store | Seven single-implementation session-store facet traits plus a SessionRepository aggregate, all backed by one PostgresSessionStore |
| 37 | 🟡 | session / db / migrations / runtime-store | moa-migrations keeps three parallel schema sources: a 24-file incremental chain, hand-curated per-domain replay subsets, and a hand-copied SQL prefix guarded by a sync test — plus an ownership manifest no tool reads |
| 38 | 🟡 | session / db / migrations / runtime-store | Duplicate plain vs control-plane analytics read paths where production only ever uses the control-plane variant |
| 39 | 🟡 | lineage / ocsf / observability | moa-lineage-sink ships four dead or test-only parallel surfaces, including a backwards-compat decode shim |
| 40 | 🟡 | skills / artifacts | Three-tier artifact visibility machinery for a scope enum with exactly one variant |
| 41 | 🟡 | skills / artifacts | Lesson-graph subsystem (learn_lesson + render addenda + SkillRegistry::load_full) has zero production callers |
| 42 | ✅ | skills / artifacts | ProcedureCondition::Expression escape-hatch variant that can only ever fail at runtime |
| 43 | 🟡 | edge / messaging / security | MCP credential proxy mints an opaque grant token that is consumed two lines later in the same function |
| 44 | 🟡 | edge / messaging / security | rate_limit.rs ships a full send-retry framework and metrics registry that no production connector uses |
| 45 | ✅ | edge / messaging / security | CredentialVault trait carried dead delete/list methods around a live set path |
| 46 | 🟡 | edge / messaging / security | Slack outbound-ref store hand-rolls a distributed lock plus a two-tier cache for edits that are already serialized upstream |
| 47 | 🟡 | Orchestrator — services/rest | OrchestratorCtx dependency-group pyramid: 10-way trait-object fan-out of one store plus three parallel accessor surfaces, mostly dead |
| 48 | ✅ | Orchestrator — services/rest | runtime/endpoint.rs builds a function-pointer service registry to express a static bind list |
| 49 | 🟡 | Orchestrator — services/rest | SessionStore service '_inner' layer: ~28 one-line pass-through methods between handlers and the store |
| 50 | 🟡 | Orchestrator — services/rest | The 'redis' cargo feature is mandatory-in-practice: every build enables it and a non-redis binary refuses to start |
| 51 | 🟡 | Orchestrator — services/rest | postmark/twilio feature flags are enabled by no build and guard ~1,450 lines of unreachable adapter code |
| 52 | ✅ | moa-brain (context pipeline) | Two overlapping compaction subsystems both own LLM checkpoint emission (stage-8 HistoryCompiler and stage-10 Compactor) |
| 53 | 🟡 | moa-brain (context pipeline) | Per-turn lineage emits a constant 'retrieval_recall_proxy' score and records cost three different ways |
| 54 | 🟡 | moa-brain (context pipeline) | Process-wide circuit-breaker registry (HashMap by namespace) with exactly one production namespace |
| 55 | 🟡 | moa-memory (graph/ingest/lifecycle/pii/vector) | Turbopuffer second vector backend with outbox, promotion state machine, and dual-read for zero tenants |
| 56 | 🟡 | moa-memory (graph/ingest/lifecycle/pii/vector) | Ingestion dependency wiring: process-global runtime with fingerprint compatibility checking plus three parallel dependency bundles and dead entry points |
| 57 | 🟡 | moa-memory (graph/ingest/lifecycle/pii/vector) | embedding_model_version: a parallel identity column that is written everywhere and read nowhere |
| 58 | ✅ | moa-memory (graph/ingest/lifecycle/pii/vector) | MemoryScope::ancestors() is an identity function feeding a vestigial scope_ancestors chain |
| 59 | ✅ | moa-memory (graph/ingest/lifecycle/pii/vector) | PiiSpan.replacement Option exists only for backwards compatibility and never holds a non-default value |
| 60 | 🟡 | moa-memory (graph/ingest/lifecycle/pii/vector) | memory.auto_bootstrap config knob has no production reader |
| 61 | ✅ | moa-core (traits/types/config/wire) | Hand-rolled ~2,300-line env-to-config mapping layer (MoaEnvOverlay + 22 apply_* mirror functions) |
| 62 | 🟡 | moa-core (traits/types/config/wire) | 119 of 245 env config knobs are never set or referenced anywhere in the repo |
| 63 | ✅ | moa-core (traits/types/config/wire) | Session persistence split into 7 single-impl 'focused contract' traits plus a blanket SessionRepository aggregate |
| 64 | ✅ | moa-core (traits/types/config/wire) | Four near-identical turn-message DTOs in wire/turn.rs, two of them field-for-field identical |
| 65 | ✅ | moa-core (traits/types/config/wire) | EmbeddingProvider carries synonym method pairs (dimensions/dimension, model_id/model_name) |
| 66 | 🟡 | lineage / ocsf / observability | LineageHandle JSON bridge forces serialize -> deserialize -> clone -> deserialize on every lineage event |
| 67 | 🟡 | auth / agents / contacts / scoring / experiments | Single-variant ActionRuleScope enum threaded through scoring, experiments, and agents |
| 68 | 🟡 | auth / agents / contacts / scoring / experiments | fga-bootstrap ships a second hand-rolled OpenFGA HTTP client that its own doc comment calls a stopgap |
| 69 | 🟡 | auth / agents / contacts / scoring / experiments | Six experiment score row types are field-identical mirrors of moa-scoring types, with six hand-written mapping functions |
| 70 | ✅ | auth / agents / contacts / scoring / experiments | Agent policy mode enums duplicated byte-for-byte between moa-artifacts and moa-core, bridged by match-mappings in the resolver |
| 71 | 🟡 | auth / agents / contacts / scoring / experiments | API-key validation uses two global caches plus per-entry mutexed revocation-recheck timestamps where one short-TTL cache gives the same guarantees |
| 72 | ✅ | auth / agents / contacts / scoring / experiments | AsyncAuthzProvider::poll_decision is a trait method with zero production callers; approvals resolve exclusively via awakeables |
| 73 | ✅ | eval crates | Entire Reporter subsystem (trait, TerminalReporter, ReporterOptions, build_reporters) has no production consumer |
| 74 | ✅ | eval crates | PairwiseLlmJudge and the AnswerJudge trait were unwired; retained by product choice and covered by an offline pairwise eval set |
| 75 | 🟡 | eval crates | MemoryOverride config knobs are pure speculative surface: two knobs hard-error 'not implemented' and the third is a no-op with a dead helper |
| 76 | 🟡 | eval crates | Dead API surface in moa-eval-core and EvalEngine: unused discovery helpers, never-constructed error variants, an uncalled engine method, and an errors-only enum variant |
| 77 | ✅ | eval crates | Pentest support is library code for exactly one test binary, with drifted env knobs that CI sets under names the code no longer reads |
| 78 | ✅ | loadtest / test-support / xtask / scripts | Duplicated Restate ingress HTTP client and session-bootstrap sequence in moa-loadtest vs moa-test-support |
| 79 | 🟡 | loadtest / test-support / xtask / scripts | check-architecture-boundaries: 99-entry counted-allowance ledger plus numeric LOC/symbol/package-count ratchets |
| 80 | 🟡 | loadtest / test-support / xtask / scripts | Generic SessionStore 'contract tests' in moa-test-support with exactly one implementation and one consumer |
| 81 | 🟡 | loadtest / test-support / xtask / scripts | perf-gate scenario layer duplicates report/gate plumbing inside moa-loadtest, including a dead public export |
| 82 | ✅ | loadtest / test-support / xtask / scripts | xtask migrate-test-db is a no-op that four CI steps build a release binary to run |
| 83 | ✅ | loadtest / test-support / xtask / scripts | inspectable_files: dead configurable workspace-root branch only ever called with None |
| 84 | 🟡 | cross-cutting: single-impl abstractions | Session-store 'focused contract' trait family: 8 single-impl traits + supertrait all backed by one PostgresSessionStore |
| 85 | ✅ | cross-cutting: single-impl abstractions | IngestionObserver trait + pipeline type parameter with one zero-sized impl used everywhere, including tests |
| 86 | ✅ | cross-cutting: single-impl abstractions | Test doubles living in production knowledge-service module, one of them dead |
| 87 | 🟡 | cross-cutting: single-impl abstractions | BranchManager trait with one impl, no dyn usage, and consumers that already name the concrete type |
| 88 | ✅ | cross-cutting: single-impl abstractions | DeliverySink trait is pure ceremony: single impl, never used as dyn or bound, consumer names the concrete type |
| 89 | 🟡 | cross-cutting: config & feature-flag sprawl | experiments and internal-eval-runner features are enabled by no build, CI job, or script — ~10k lines of never-compiled subsystem |
| 90 | 🟡 | cross-cutting: config & feature-flag sprawl | 150 of ~245 MOA_* env knobs are write-only: supported by MoaEnvOverlay but never set anywhere in the repo |
| 91 | ✅ | cross-cutting: config & feature-flag sprawl | slack/postmark/twilio feature triple-gating means every built binary ships without messaging, while the env surface pretends otherwise |
| 92 | 🟡 | cross-cutting: config & feature-flag sprawl | Daytona/E2B hand-provider features are enabled by nothing, making 1,700 lines of adapters and their config knobs unreachable in every artifact |
| 93 | ❌ | cross-cutting: config & feature-flag sprawl | Neon branching stack (~1,300 lines + 9 env knobs) is configured off in every environment |
| 94 | 🟡 | cross-cutting: config & feature-flag sprawl | Vestigial auth0 feature layer: an inner feature flag that gates nothing, on a provider chain no artifact ever ships |
| 95 | ✅ | cross-cutting: cross-crate duplication | Retry-After parsing, retryable-status classification, jitter, and response-body helpers are implemented independently in four crates |
| 96 | ✅ | cross-cutting: cross-crate duplication | Ephemeral test-Postgres bootstrap (URL fallback + identifier quoting + search-path pool + schema migrations) copy-pasted across ~14 test-support files |
| 97 | ✅ | cross-cutting: cross-crate duplication | ~35 hand-rolled mock LLMProvider implementations with a 25-line ModelCapabilities/pricing blob duplicated 29 times, alongside an existing general-purpose ScriptedProvider |
| 98 | 🟡 | cross-cutting: cross-crate duplication | SQL identifier quoting defined 19 times and the search-path pool builder duplicated across three production binaries despite moa-db existing as the shared storage-helpers crate |
| 99 | ✅ | cross-cutting: cross-crate duplication | memory_llm::LlmChatClient re-implements retry, jitter, error classification, and provider failover inside moa-providers, parallel to the crate's own RetryPolicy/RateGuard/FailoverLLMProvider stack |
| 100 | ❌ | session / db / migrations / runtime-store | Neon checkpoint/branch subsystem (BranchManager + NeonBranchManager + NeonMaint cron + 4 admin handlers) that is never enabled anywhere |
| 101 | ❌ | lineage / ocsf / observability | Fjall fsync journal wraps the lossy fire-and-forget lineage path that is allowed to drop events anyway |

---

## ✅ Confirmed findings

_An independent adversarial reviewer read the code and could not refute these._

### 1. Progress-emit plumbing threads two dead parameters and a cadence struct through every call site

**Area:** Orchestrator — workflows & turn_driver
effort: **small** · finder confidence: **high** · ~LOC removable: **~90**

**Locations**

- `crates/moa-orchestrator/src/workflows/turn_progress.rs:139-156`
- `crates/moa-orchestrator/src/turn_driver/progress.rs:57-77`
- `crates/moa-orchestrator/src/tool_invocation/governed.rs:44-53,266-273,353-360`
- `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`

**What it is.** `turn_progress::maybe_emit(ctx, session_id, _turn_id, _phase, summary, first_delay_ms, interval_ms)` ignores its `_turn_id` and `_phase` parameters, yet all ~12 call sites across turn_execution.rs, worker_turn_execution.rs and governed.rs dutifully compute and pass a turn id and TurnPhase. Each call site also first calls `driver_progress::current_cadence()` (which wraps `cadence_from_limits`, which wraps two config fields into a `ProgressCadence` struct) and then unpacks it into two u64 arguments. To carry those numbers across the module boundary into the governed tool executor, a dedicated `GovernedInvocationProgress { turn_id, first_delay_ms, interval_ms }` struct exists whose only consumers are the two `maybe_emit` calls that ignore `turn_id`.

**Why it may be over-engineered.** Two of five parameters are dead weight at every call site, and the cadence values are global config (`OrchestratorCtx::current_config().session_limits`) that `maybe_emit` could read itself — there is no per-caller cadence variation anywhere. `ProgressCadence`, `cadence_from_limits`, `current_cadence`, and `GovernedInvocationProgress` exist solely to move two constants to a function that could fetch them in one line.

**Simpler alternative.** Change the signature to `maybe_emit(ctx, session_id, summary)` reading `progress_first_delay_ms`/`progress_interval_ms` from `OrchestratorCtx::current_config()` internally. Delete `ProgressCadence`, `cadence_from_limits`, `current_cadence`, the `GovernedInvocationProgress` struct (and its field in `GovernedInvocationRequest`), the two-line cadence preamble at every call site, and the unused turn_id/phase arguments.

**Side effects / what to watch.** One unit test (`progress_cadence_uses_session_limits_directly`) is deleted or rewritten; if a per-caller cadence is ever wanted later it must be reintroduced (no evidence of that today).

**Value of simplifying.** Removes ~80-100 lines of pure parameter plumbing and one struct per module boundary; every progress emit becomes a single self-explanatory call.

**Adversarial verifier: ✅ CONFIRMED.** Every factual assertion checked out against the code. (1) Dead parameters: crates/moa-orchestrator/src/workflows/turn_progress.rs:139-147 defines maybe_emit(ctx, session_id, _turn_id, _phase, summary, first_delay_ms, interval_ms); _turn_id and _phase are underscore-prefixed and never used in the body (lines 148-155 only use session_id, summary, and the two cadence numbers; progress_delivery::maybe_deliver at workflows/progress_delivery.rs:41 takes only (ctx, session_id, summary) — phase is used only by the separate maybe_deliver_terminal path, which is untouched by the proposal). (2) Uniform cadence: all 11 maybe_emit call sites (turn_execution.rs:637,723,848,940,1459,1724,1941; worker_turn_execution.rs:319,619; governed.rs:266,353) get their cadence exclusively from driver_progress::current_cadence(), which is cadence_from_limits(&OrchestratorCtx::current_config().session_limits) (turn_driver/progress.rs:65-77). No call site ever varies the cadence; the governed.rs sites receive it via GovernedInvocationProgress populated from current_cadence() at turn_execution.rs:1860-1877 and worker_turn_execution.rs:520-540. (3) GovernedInvocationProgress (governed.rs:44-53) fields are consumed only at governed.rs lines 269/272/273 and 356/359/360 — i.e., fed into the two maybe_emit calls that ignore turn_id — so the whole struct is dead weight. (4) No hidden consumers: workspace-wide grep plus a grep of moa-orchestrator/tests/, docs/, and scripts/ found zero other references to ProgressCadence, cadence_from_limits, current_cadence, or GovernedInvocationProgress; all symbols are pub(crate), so nothing outside the crate can depend on them. The config knobs progress_first_delay_ms/progress_interval_ms live in moa-core config (context.rs, env_overlay.rs) and survive the change untouched. (5) Load-bearing constraints do not force the plumbing: reading OrchestratorCtx::current_config() inside maybe_emit is deterministically identical to the status quo, because every caller already reads current_cadence() inline in workflow code (not inside ctx.run), often calling OrchestratorCtx::current_config() in the very same function (e.g. turn_execution.rs:649 right after the maybe_emit at :638); the replay/non-determinism surface is unchanged, and the durable gating state is persisted via ctx.set regardless. The testable pure core (ProgressState::attempt, which legitimately takes first_delay_ms/interval_ms) is untouched by the proposal, so the six behavior-pinning unit tests in turn_progress.rs survive. Minor side effects the claimant missed, all mechanical compile fallout with no behavioral pin: the governed.rs test helper request() at governed.rs:652-673 constructs GovernedInvocationProgress{turn_id:"turn-1",100,200} and must drop the field (its 8 tests pin policy/request shape, not progress, and never call maybe_emit); the now-unused SessionLimitsConfig import in turn_driver/progress.rs and possibly TurnPhase imports at some call sites need cleanup; and the deleted test is progress_cadence_uses_session_limits_directly at turn_driver/progress.rs:231 as claimed. The proposal is strictly simpler: it deletes a struct, two functions, a struct field, two dead args at ~11 sites, and an 11x-repeated two-line preamble, replacing them with one config read inside maybe_emit.

---

### 2. turn_progress duplicates state loaders and journaled clocks per context type and spreads one struct over four Restate keys

**Area:** Orchestrator — workflows & turn_driver
effort: **small** · finder confidence: **high** · ~LOC removable: **~70**

**Locations**

- `crates/moa-orchestrator/src/workflows/turn_progress.rs:177-251`
- `crates/moa-orchestrator/src/workflows/turn_progress.rs:10-13,223-235`
- `crates/moa-orchestrator/src/workflows/mod.rs:32-41`

**What it is.** `load_workflow_state(&WorkflowContext)` and `load_shared_state(&SharedWorkflowContext)` are two byte-identical 20-line functions; `workflow_utc_now` and `shared_utc_now` are two copies of the already-existing `workflows::durable_utc_now` helper differing only in context type and step name. The `ProgressState` struct (4 fields) is persisted as four separate Restate state keys, so every `maybe_emit` performs 4 `ctx.get` journal reads, one journaled `ctx.run` clock sample, and up to 4 `ctx.set` writes — on every model-call and tool-call boundary of every turn.

**Why it may be over-engineered.** restate-sdk 0.10 provides blanket `ContextReadState`/`ContextSideEffects` impls covering both `WorkflowContext` and `SharedWorkflowContext` (turn_progress already calls `ctx.run` on the shared context), so a single generic `fn load_state<C: ContextReadState>` and reuse of `durable_utc_now` (generalized over `ContextSideEffects`) eliminate all three duplicate pairs. Storing one `Json<ProgressState>` under one key halves the per-emit journal traffic with no behavioral change — the four keys are never read individually.

**Simpler alternative.** One `const K_PROGRESS: &str` holding `Json<ProgressState>` (derive Serialize/Deserialize); one generic loader over `ContextReadState`; delete `load_shared_state`, `shared_utc_now`, `workflow_utc_now` and call a context-generic `durable_utc_now`. `snapshot()` and `maybe_emit()` keep their signatures.

**Side effects / what to watch.** Renames the durable state keys and the clock step names — in-flight workflow journals would not replay cleanly across a deploy, which is acceptable pre-prod. No wire or event changes.

**Value of simplifying.** Deletes ~70 duplicated lines, removes a triple-maintained clock helper, and cuts ~6 journal entries per progress attempt on the hottest workflow path (turn loop), which docs/18-performance.md identifies as the optimization target area.

**Adversarial verifier: ✅ CONFIRMED.** Every factual element of the claim checks out against the real code. (1) crates/moa-orchestrator/src/workflows/turn_progress.rs:177-198 (load_workflow_state) and :200-221 (load_shared_state) are byte-identical except for the context parameter type; :237-243 (workflow_utc_now) and :245-251 (shared_utc_now) repeat the exact ctx.run(Utc::now) pattern of workflows::durable_utc_now (crates/moa-orchestrator/src/workflows/mod.rs:32-41), which already takes step_name as a parameter. (2) SDK support verified in the actual dependency source: Cargo.lock pins restate-sdk 0.10.0, and ~/.cargo/registry/.../restate-sdk-0.10.0/src/context/mod.rs contains a blanket `impl<'ctx, CTX: SealedContext<'ctx>> ContextSideEffects<'ctx> for CTX {}` plus `impl<CTX: SealedContext + SealedCanReadState> ContextReadState for CTX {}` with explicit `impl SealedCanReadState for SharedWorkflowContext<'_>` and for WorkflowContext; RunFuture::name exists (run.rs:39). Both traits are public (sealed only against downstream impls), so generic fns bounded on them compile. (3) No hidden consumers: workspace-wide grep for the four state keys (progress_started_at, progress_last_emitted_at, progress_elapsed_ms, progress_last_summary) and both step names hits only turn_progress.rs itself; the keys are never read individually — both loaders read all four together; the session_turn_lifecycle_service_e2e test asserts on HTTP progress responses, not raw Restate state keys; no scripts/docs introspect these keys. (4) No load-bearing constraint: docs/02-brain-orchestration.md only requires side effects be journaled via ctx.run (preserved by the proposal); nothing pins the key layout, and the mod.rs warning about stable step names is satisfiable because the names can be passed through unchanged. maybe_emit is indeed on the hot path of every model/tool boundary (turn_execution.rs:638,724,849,941,1460,1725,1942; worker_turn_execution.rs:320,620; tool_invocation/governed.rs:266,353), so halving journal traffic per emit is real. Bonus supporting evidence: a third near-duplicate durable_utc_now exists at crates/moa-orchestrator/src/objects/mod.rs:14 (ObjectContext), which a context-generic helper would also subsume, and merging four keys into one Json<ProgressState> makes the shared-handler snapshot read atomic instead of four separate reads. Minor correction absorbed into side effects: the clock step names do NOT need to change (durable_utc_now takes step_name; pass "turn_progress_utc_now" / "turn_progress_snapshot_utc_now" through), so the only durable-journal break is the state-key consolidation — acceptable pre-prod and invisible to all tests and tooling. ProgressState just needs Serialize/Deserialize derives (chrono serde support already in use via Json<DateTime<Utc>> in this file).

---

### 3. BrainResponse lineage record re-fetched by fuzzy text match instead of by its known sequence number

**Area:** Orchestrator — workflows & turn_driver
effort: **small** · finder confidence: **high** · ~LOC removable: **~25**

**Locations**

- `crates/moa-orchestrator/src/workflows/turn_execution.rs:2621-2646`
- `crates/moa-orchestrator/src/workflows/turn_execution.rs:986-1003`

**What it is.** After `append_brain_response_from_completion` persists the assistant response and returns its exact `sequence_num`, the very next step calls `latest_matching_brain_response_event`, which issues a second SessionStore `get_events(EventRange::recent(8))` call and scans the last 8 events for a `BrainResponse` whose `text` and `model` equal the response, taking the max sequence — purely to hand the `EventRecord` to `emit_generation_lineage`.

**Why it may be over-engineered.** The workflow already holds the authoritative sequence number of the row it just wrote; re-discovering it by content equality over a recent window is an indirection with real failure modes (identical retry texts matching the wrong row, the row falling outside the 8-event window when tools/workers append concurrently) and an extra service hop with a full 8-event payload on every model iteration.

**Simpler alternative.** Fetch exactly `EventRange { from_seq: Some(response_sequence_num), to_seq: Some(response_sequence_num), .. }` (one row, same identity-header path), or extend the append handler to return the `EventRecord` it created and pass that straight to lineage — deleting `latest_matching_brain_response_event` entirely.

**Side effects / what to watch.** None functional; lineage gets a strictly more reliable event reference. One fewer get_events call per turn iteration (visible in TurnMetrics counters and coordination tests that assert call counts, which may need count updates).

**Value of simplifying.** Removes a fuzzy-matching moving part and a redundant per-iteration event fetch from the hot turn loop; eliminates a class of wrong-row lineage bugs.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy verified: crates/moa-orchestrator/src/workflows/turn_execution.rs:986-1003 shows `append_brain_response_from_completion` (L766-791, which wraps the shared `append_session_event` in crates/moa-orchestrator/src/workflows/turn_events.rs:28-48) returning the authoritative u64 sequence_num of the just-written BrainResponse, immediately followed at L997 by `latest_matching_brain_response_event` (L2621-2646), which issues a second SessionStore `get_events(EventRange::recent(8))` and scans for `text == response.text && model == response.model`, taking max sequence_num — exactly as claimed. The appended text is `response.text.clone()` from the same `visible_response` (L778), so the fuzzy match adds no information the workflow doesn't already have. Hidden consumers: grep shows exactly one caller of `latest_matching_brain_response_event` (L997); no tests, docs, scripts, or other crates reference it, so deletion is safe. What lineage actually needs: crates/moa-brain/src/lineage.rs:188-189 consumes both `record.id` (Uuid) and `record.sequence_num`, and passes the record to build_citation_lineage — so some fetch (or an extended append response) is still required to obtain the event Uuid; the claim's proposed single-row fetch covers this. The proposed alternative is a proven existing pattern in this codebase: crates/moa-orchestrator/src/brain_bridge.rs:227-239 already fetches a known event with `EventRange { from_seq: Some(n), to_seq: Some(n), event_types: Some(...) }`, and turn_execution.rs itself uses exact from_seq ranges at L2506/2530/2552 — EventRange (crates/moa-core/src/types/events_stream.rs:16-25) natively supports it. Load-bearing constraints checked and none force the complexity: Restate determinism is unaffected (both variants are journaled service calls, and an exact-seq fetch of an immutable row is strictly MORE replay-deterministic than a recent-window scan whose first-execution contents depend on concurrent worker/guardrail appends); no docs/02, docs/18, or AGENTS.md mandate mentions this lookup; the identity-header path is preserved. Failure modes claimed are real: recent(8) returns None (silently nulling lineage's response_event_id/sequence_num) if >8 events land between append and fetch (concurrent workers appending to the same session), and identical retry text+model can match a concurrently appended row. Side-effect check: the claimant slightly overstated test impact — crates/moa-orchestrator/tests/coordination_cost_service_e2e.rs:174 asserts `cost.get_events_calls <= 4` as an upper-bound ceiling, so removing one call per iteration requires no test change (it only adds headroom). One nuance on the second alternative: `append_event`'s wire response (u64) is shared by delegation.rs, governed.rs, and both turn workflows, and Restate journals every response, so returning a full EventRecord from every append inflates journal entries workspace-wide; the single-row exact-range fetch is the cleaner of the two proposals and matches the established brain_bridge pattern.

---

### 4. Dead classifier input is_workflow_context and test-only production helper

**Area:** Orchestrator — workflows & turn_driver
effort: **small** · finder confidence: **high** · ~LOC removable: **~35**

**Locations**

- `crates/moa-orchestrator/src/workflows/turn_responsiveness.rs:18-19,50`
- `crates/moa-orchestrator/src/turn_driver/model_loop.rs:64,81`
- `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs:981-991,1317-1343`

**What it is.** `TurnResponsivenessInput.is_workflow_context` is a classifier knob that is set to `false` at both production construction sites (root and worker loop plans in model_loop.rs) and never set to `true` anywhere in the workspace; the branch `if input.is_workflow_context || input.is_worker_context` therefore reduces to the worker flag. Separately, `parent_session_from_initial_message` in worker_turn_execution.rs is compiled only under `#[cfg(test)]` and exists solely so its own unit test (`reserved_child_parent_session_requires_initial_message`) can call it — it pins no production code path.

**Why it may be over-engineered.** The workflow-context flag is speculative extensibility left over from a procedure-adapter turn path that now flows through explicit `request_max_turns` instead; keeping it forces every constructor and the classifier test-input builder to carry a field that can never influence behavior. A test that asserts the behavior of a function that only exists for the test is pure tautology and gives false coverage confidence.

**Simpler alternative.** Delete the `is_workflow_context` field and fold the classifier branch to `if input.is_worker_context`; delete `parent_session_from_initial_message` together with its test (the real spawn-path validation lives in the Worker VO admission code).

**Side effects / what to watch.** None at runtime; a couple of classifier unit tests drop a field from their input literals.

**Value of simplifying.** Removes a dead config surface from the turn classifier (one less thing to reason about when tuning responsiveness) and a fake test; small but zero-risk.

**Adversarial verifier: ✅ CONFIRMED.** 1) Factual accuracy verified against the real code. `is_workflow_context` is declared at crates/moa-orchestrator/src/workflows/turn_responsiveness.rs:19, consumed only in the branch at :50 (`if input.is_workflow_context || input.is_worker_context`). A workspace-wide grep over .rs/.toml/.md/.sh/.yml/.yaml/.json found exactly five other occurrences: the `#[cfg(test)]` `root()` builder (:35, false), the classifier's own unit test `workflow_shaped_request_is_complex` (:817, the only `true` anywhere), and both production construction sites in crates/moa-orchestrator/src/turn_driver/model_loop.rs — `root_loop_plan` (:64) and `worker_loop_plan` (:81), both hardcoded `false`. No docs, scripts, config, or compose references. In production the branch provably reduces to `input.is_worker_context`, which `worker_loop_plan` does set true, so the worker flag stays. 2) `parent_session_from_initial_message` at crates/moa-orchestrator/src/workflows/worker_turn_execution.rs:981-991 is gated `#[cfg(test)]` and its only references are the tests-mod import at :1189 and its own test `reserved_child_parent_session_requires_initial_message` (:1317-1343); graphify confirms the only graph edge is to that test. The claim's assertion that real spawn-path validation lives in Worker VO admission checks out: crates/moa-orchestrator/src/objects/worker/state.rs:190-192 rejects non-InitialTask initialization in production ("worker initialization requires an InitialTask message"), so the test pins a copy, not the shipped path — tautological coverage. 3) No load-bearing constraint forces the flag: `classify_turn_request` is a pure deterministic function called inside plan construction; removing a constant-false input cannot perturb Restate journal entries differently than any other code change, and MOA is pre-prod with an explicit no-backwards-compat policy, so in-flight-replay compatibility is waived by project rules. 4) The simplification works: fold :50 to `if input.is_worker_context`, drop the field from the struct, `root()`, and both model_loop.rs literals; delete the cfg(test) helper plus its test. No import fallout — `TerminalError` is still used in production at worker_turn_execution.rs:503, and `HandlerError`/`SessionId` are used throughout. 5) One side effect the claimant slightly understated: the test `workflow_shaped_request_is_complex` (turn_responsiveness.rs:814-832) loses its entire first assertion block (the `is_workflow_context: true` input), not merely a field from a literal, and its `// Pins:` comment must drop the "explicit workflow contexts" wording; the second half (text-shaped "Task Goal ... workflow" prompt classifying Complex) remains valid and keeps that test alive. This is cosmetic, not a runtime or coverage regression.

---

### 5. Two parallel pipeline assemblies: test-only build_default_pipeline duplicates the production stage list in the public API

**Area:** moa-brain (context pipeline)
effort: **small** · finder confidence: **medium** · ~LOC removable: **~50**

**Locations**

- `crates/moa-brain/src/pipeline/builder.rs:29-80`
- `crates/moa-brain/src/lib.rs:22-27`

**What it is.** builder.rs exports build_default_pipeline and build_default_pipeline_with_tools, whose own doc comment says production uses the graph-memory builder and these 'remain useful for isolated pipeline and brain-loop tests'. Grep confirms every caller outside moa-brain/src is a moa-brain test file (brain_turn_offline, cache_audit_offline, brain_turn_session_search_db, brain_turn_artifacts_db, ...). The function re-assembles a trimmed copy of the production stage list (Identity, AgentInstruction, Instruction, ToolDef, History, DelegationPlanning, RuntimeContext, Compactor) that must be manually kept in sync with the real builder at lines 153-274.

**Why it may be over-engineered.** A near-duplicate assembly path exported as production API purely for tests. When a stage is added or reordered in the production builder, the test builder silently diverges, so brain-turn tests exercise a pipeline shape production never runs.

**Simpler alternative.** Make the memory legs of the single graph builder optional (graph_pool/skill-injector/digest legs skipped when absent, mirroring how query_rewrite is already optional) so tests call the same assembly with fewer options; or move the trimmed builder into tests/support and drop it from lib.rs. Either way there is exactly one stage-list literal in the crate.

**Side effects / what to watch.** Mechanical import updates across ~6 brain test harness files and the eval-harness example; if graph_pool becomes Option, the builder signature changes for moa-orchestrator's production call site too.

**Value of simplifying.** One source of truth for pipeline composition; test pipelines can no longer drift from the production stage order; two public functions removed from the crate API.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy: verified. crates/moa-brain/src/pipeline/builder.rs:29-80 contains build_default_pipeline/build_default_pipeline_with_tools whose own doc comment says production uses the graph-memory builder and these "remain useful for isolated pipeline and brain-loop tests". Both are re-exported at crate root (crates/moa-brain/src/lib.rs:22-27 and pipeline/mod.rs:20-25). Graphify caller traversal plus a workspace-wide grep (*.rs, *.md, *.toml, *.yml, *.sh) found zero non-test consumers: the only callers are crates/moa-brain/tests/{brain_turn_offline.rs, cache_audit_offline.rs, brain_turn_artifacts_db.rs, brain_turn_session_search_db.rs} and their include!-ed brain_turn_support/{common,offline,artifacts,session_search}.rs files (7 test files total). No orchestrator, edge, eval, loadtest, example, script, compose, or doc reference exists. The trimmed builder duplicates the shared stage prefix/suffix (Identity, AgentInstruction, Instruction, ToolDef / History, DelegationPlanning, RuntimeContext, Compactor) of the production builder at builder.rs:153-274 with no test pinning equivalence, so silent divergence is possible; it also skips the record_context_pipeline_construction metric the production path emits. Load-bearing constraints: none found. The orchestrator production path (crates/moa-orchestrator/src/brain_bridge.rs) uses only the graph builder, so Restate determinism (docs/02) is untouched; performance instrumentation (docs/18) lives only in the graph builder; AGENTS.md actually argues FOR the claim ("every test must exercise a real production path", rule 7 against path-preserving exports). Simpler alternative viability: both proposed options work. Option B (move trimmed assembly into tests/brain_turn_support and drop the lib.rs/pipeline exports) is the cheaper one and is confirmed feasible without expanding public surface: every ingredient is already public — all stage modules are `pub mod` in pipeline/mod.rs, constructors are pub (HistoryCompiler::new/with_compaction_config/with_tool_output_config/with_snapshot_config in pipeline/history/mod.rs, Compactor::new in pipeline/compactor/mod.rs, ToolDefinitionProcessor::new, IdentityProcessor, etc.), and ContextPipeline::with_runtime_limits is pub (pipeline/runner.rs:38). Option A (graph_pool: Option) also works — the pool-dependent legs (SharedGraphMemoryRetriever, SkillInjector, DigestProcessor) would become conditional, mirroring the already-optional query_rewrite leg — but its blast radius is larger than the claimant stated: besides brain_bridge.rs, the graph-builder signature change touches crates/moa-eval/src/setup.rs, crates/moa-brain/examples/chat_harness.rs, and ~5 moa-brain _db_memory/_live test files (cache_audit_live.rs, brain_turn_live.rs, brain_turn_cache_replay_db_memory.rs, stable_prefix_db_memory.rs, skill_package_materialization_db_memory.rs, brain_db_memory/pipeline_stages_db_memory.rs) — all mechanical Some(pool) wraps. One nit: no "eval-harness example" uses the trimmed builder (src/harness takes a pre-built pipeline), so option B's side effects are confined entirely to moa-brain test files. Recommend option B.

---

### 6. Dead parallel env-based provider construction path in registry/descriptors

**Area:** moa-providers
effort: **small** · finder confidence: **high** · ~LOC removable: **~60**

**Locations**

- `crates/moa-providers/src/registry.rs:101-114`
- `crates/moa-providers/src/registry.rs:507-511`
- `crates/moa-providers/src/routing.rs:55-56`
- `crates/moa-providers/src/routing.rs:85-89`
- `crates/moa-providers/src/routing.rs:187-197`

**What it is.** Each ProviderDescriptor carries two factory function pointers (build_from_env + build_from_config) and a default_api_key_env field. ProviderRegistry::from_env() iterates descriptors, checks the raw env var via configured_env(), and registers env-based factories — a full parallel construction path next to from_config().

**Why it may be over-engineered.** ProviderRegistry::from_env() has zero callers anywhere in the workspace (grep confirms; every production/test registry is built via from_config, with_static_providers, scripted, or mock). MoaConfig already loads from the same MOA_* env vars, so the env path duplicates config loading. build_from_env, default_api_key_env, the EnvProviderFactory typedef, configured_env(), and the three build_*_provider free functions exist solely to feed this dead method. The provider-level from_env constructors (AnthropicProvider::from_env etc.) are used only by one live test that could construct via config.

**Simpler alternative.** Delete ProviderRegistry::from_env(), configured_env(), the build_from_env and default_api_key_env descriptor fields, the EnvProviderFactory typedef, and the three build_*_provider free functions in routing.rs. Optionally fold the provider-level from_env constructors into from_config_with_model and update tests/provider_matrix_live.rs to build a MoaConfig (or keep just those three ctors as test conveniences).

**Side effects / what to watch.** None in production. tests/provider_matrix_live.rs keeps working if the provider-level from_env ctors are retained; otherwise it needs a ~5-line change to construct via config.

**Value of simplifying.** Removes an entire untested parallel construction path and one of the two factory slots on every descriptor; future provider additions wire one factory instead of two.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy verified by reading the cited code. crates/moa-providers/src/registry.rs:101-114 defines ProviderRegistry::from_env() iterating PROVIDER_DESCRIPTORS and gating on configured_env(descriptor.default_api_key_env) (registry.rs:507-511, a private fn reading std::env::var directly). crates/moa-providers/src/routing.rs:56 defines EnvProviderFactory, routing.rs:85/89 the default_api_key_env and build_from_env descriptor fields, and routing.rs:187-197 the three build_*_provider free functions wrapping AnthropicProvider/OpenAIProvider/GeminiProvider::from_env — exactly as claimed.

Dead-code claim verified: a repo-wide grep (rs/toml/md/sh/yml) for "ProviderRegistry::from_env" returns zero hits — no caller anywhere, including moa-providers' own tests. build_from_env, default_api_key_env, EnvProviderFactory, and the three build_*_provider free functions appear only in routing.rs descriptor literals and registry.rs:106/109, i.e. solely feeding the dead method. ProviderDescriptor/PROVIDER_DESCRIPTORS have no consumers outside crates/moa-providers/src at all, so removing the two fields breaks nothing downstream.

Hidden consumers ruled out: the configured_env() functions in crates/moa-orchestrator/tests/{llm_gateway_provider_e2e.rs:62, coordinator_worker_behavior_provider_e2e.rs:101, integration/session_vo_e2e.rs:56, integration/session_brain_e2e.rs:48} that graphify surfaced are independent local copies defined in each test file (registry.rs's configured_env is private, not exported), so they are unaffected. No scripts/, docs/, docker-compose, or nextest references to the env path exist (only .config/nextest.toml pinning the provider_matrix_live binary name, which the proposal keeps).

Redundancy claim verified: crates/moa-core/src/config/env_overlay.rs:41-45 (envy::prefixed("MOA_")) maps the exact same MOA_ANTHROPIC_API_KEY / MOA_OPENAI_API_KEY / MOA_GOOGLE_API_KEY vars into MoaConfig, and from_config() gates on configured_secret((descriptor.api_key)(&config)) — so from_env duplicates config loading with a strictly weaker result (the from_env ctors, e.g. anthropic/mod.rs:130-136, skip config-derived web_search_enabled, rate-limit pacing, and concurrency caps that from_config_with_model applies at anthropic/mod.rs:115-126). If anything the env path is not just dead but worse-configured.

Load-bearing constraints: none apply. Registry construction is process-startup wiring (moa-orchestrator/src/main.rs loads MoaConfig then builds deps), not inside Restate durable handlers, so replay determinism (docs/02) is untouched; no perf, security/PII, or test-lane requirement forces the parallel path. AGENTS.md rule 7 ("no compatibility shims") and the pre-prod no-backwards-compat posture actively favor deletion.

Provider-level from_env ctors: exactly one consumer, crates/moa-providers/tests/provider_matrix_live.rs:93-99 (three call sites). The claimant's option to retain those three ctors as live-test conveniences (or a ~5-line MoaConfig-based rewrite) is accurate; either way the descriptor-level env path deletes cleanly. Claimed side effects ("none in production") are correct; I found no missed breakage.

---

### 7. Unused Gemini EmbedRole variants driven by a never-set config knob

**Area:** moa-providers
effort: **small** · finder confidence: **high** · ~LOC removable: **~70**

**Locations**

- `crates/moa-providers/src/embedding/gemini.rs:36-86`
- `crates/moa-providers/src/embedding/factory.rs:264-283`
- `crates/moa-core/src/config/memory.rs:243-250`
- `crates/moa-core/src/config/env_overlay.rs:209`

**What it is.** EmbedRole is a 9-variant enum (Document, SearchQuery, QuestionAnsweringQuery, FactCheckingQuery, CodeRetrievalQuery, Classification, Clustering, SentenceSimilarity, Raw) with per-variant prompt-prefix formatting, parsed from the memory.vector.embedder.gemini.default_role config string via parse_embed_role.

**Why it may be over-engineered.** Production only ever uses SearchQuery (the default_role default, 'search_query') and Document{title: None} (ingestion). The config knob is never set to a non-default value anywhere — no .env.example, compose file, doc, or script references it. The other seven variants, their format arms, seven parse_embed_role arms, the env-overlay field, and the role-prefix test rows exist only to support a hypothetical config value.

**Simpler alternative.** Reduce EmbedRole to Document and SearchQuery (or replace with the existing two-variant EmbedderConstructionRole), delete parse_embed_role's extra arms, and drop the gemini.default_role config field plus its env overlay. Re-add roles when a retrieval experiment actually needs one.

**Side effects / what to watch.** Loses the ability to switch Gemini query embedding role via env without a code change — acceptable pre-prod since vectors are pinned to one embedding space anyway (changing role changes the query space and would degrade a populated index silently).

**Value of simplifying.** Removes a config knob nobody sets, seven dead enum variants and their parsing/formatting/tests, and shrinks the config surface.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy: verified exactly as claimed. crates/moa-providers/src/embedding/gemini.rs:37-59 defines the 9-variant EmbedRole with per-variant format arms at lines 64-85; crates/moa-providers/src/embedding/factory.rs:264-279 has the 9-arm parse_embed_role; crates/moa-core/src/config/memory.rs:243-250 defines gemini.default_role defaulting to "search_query"; crates/moa-core/src/config/env_overlay.rs:208-209 is the MOA_MEMORY_VECTOR_EMBEDDER_GEMINI_DEFAULT_ROLE overlay field. Factory mapping (factory.rs:246-249): Ingestion is hardcoded to Document{title: None}; only Retrieval consults the config knob.

Hidden consumers: none found. Workspace-wide grep shows EmbedRole is constructed only as SearchQuery or Document{title: None} in production-path code and tests (crates/moa-providers/tests/gemini_embedding_live.rs:40, tests/providers_offline/gemini_embedding_offline.rs:26,63,94). The knob is not set anywhere: no hit in .env, .env.example, .env.fga, .envrc, any *.toml/*.yml/*.sh/compose file, or docs; the only references to memory_vector_embedder_gemini_default_role are its own definition and the overlay application (memory.rs:317-318). docs/04-memory-architecture.md:106 explicitly documents only two prefixes in use: document (ingestion) and search-query (retrieval). Additionally, the per-call override method embed_as (gemini.rs:142) has zero callers outside gemini.rs — it exists only to serve the multi-role design and is itself dead code, which strengthens the claim.

Load-bearing constraints: none apply. This is a stateless HTTP provider adapter — no Restate replay/determinism involvement (docs/02), no perf work (docs/18), no security/PII surface (docs/08), and no test-lane requirement forces the variants; the role-prefix test rows at gemini.rs:307-350 pin only the speculative variants themselves.

Simpler alternative viability: works and is genuinely simpler. The factory already maps the two-variant EmbedderConstructionRole 1:1 onto the two used EmbedRole values, so collapsing to two roles (or formatting directly off EmbedderConstructionRole) removes parse_embed_role, the config field, the env-overlay field, and seven format arms without moving complexity anywhere. Consumers (moa-orchestrator/src/runtime/deps.rs:295, services/memory/retrieval.rs:207, services/knowledge/ingest.rs:156, moa-brain/src/pipeline/builder.rs:118) all go through build_embedder_from_config with EmbedderConstructionRole and are untouched. Fits repo philosophy: pre-prod, no backwards compat, delete speculative surface.

Side effects: the claimant's stated side effect (losing env-switchable query role) is accurate and correctly scoped — the knob never touched the ingestion side, so a populated index's document space was always pinned anyway. Minor additional cleanup the claimant missed (all in the same direction): (a) embed_as at gemini.rs:142 is uncalled and should go too, (b) Document's title field is Option<String> but is only ever None, so the field can be dropped, (c) the two offline tests and one live test that pass EmbedRole::SearchQuery/Document{title: None} need a mechanical constructor update, and (d) EmbedRole is re-exported from moa_providers lib.rs:38 and embedding/mod.rs:13, so the public export shrinks or disappears — no external consumers exist.

**Implementation status: ✅ DONE.** Current code has no public `EmbedRole`, `parse_embed_role`, `embed_as`, `memory.vector.embedder.gemini.default_role`, or `MOA_MEMORY_VECTOR_EMBEDDER_GEMINI_DEFAULT_ROLE` path. `GeminiEmbeddingEmbedder::new` now takes the existing two-variant `EmbedderConstructionRole` directly, and the same patch deleted the dead nested vector-embedder API-key mirror structs (`cohere`, `gemini`, `zeroentropy`) because provider construction reads the canonical `providers.*.api_key` fields. Verification passed with `cargo test -p moa-providers --lib role_prefixes_match_documented_shapes --locked`, `cargo test -p moa-providers --test providers_offline --locked gemini_embedding_offline -- --nocapture`, `cargo test -p moa-core --lib env_only_loads_memory_extraction_and_provider_config --locked`, `cargo test -p moa-core --lib from_iter_applies_flat_single_underscore_env --locked`, and `cargo check -p moa-core -p moa-providers -p moa-memory-ingest -p moa-brain -p moa-orchestrator --all-targets --locked`.

---

### 8. Speculative unwired fallback chain inside LlmChatClient

**Area:** moa-providers
effort: **small** · finder confidence: **high** · ~LOC removable: **~30**

**Locations**

- `crates/moa-providers/src/memory_llm/client.rs:26-30`
- `crates/moa-providers/src/memory_llm/client.rs:78-107`

**What it is.** LlmChatClient (the Cohere chat transport for memory ingestion) carries a fallbacks: Vec<LlmChatClient> field, a with_fallback() builder, and a retry-then-iterate-fallbacks loop in chat().

**Why it may be over-engineered.** with_fallback has zero callers in the entire workspace — the doc comment itself admits construction sites are 'deliberately left unwired'. The fallback loop in chat() is dead code that adds a second failover mechanism to a crate that already has FailoverLLMProvider for the chat path, and it swallows fallback errors (if let Ok) in a path that can never execute.

**Simpler alternative.** Delete the fallbacks field, with_fallback(), and the fallback loop; chat() becomes a direct call to chat_with_retry(). Re-introduce a chain if/when an ingestion construction site actually wires one.

**Side effects / what to watch.** None — no callers exist; existing wiremock tests for the client are unaffected.

**Value of simplifying.** Removes dead speculative machinery and a second, untested failover concept from the ingestion transport.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy verified by reading crates/moa-providers/src/memory_llm/client.rs: the `fallbacks: Vec<LlmChatClient>` field (line 29), `with_fallback()` builder (lines 84-87), and the retry-then-iterate-fallbacks loop in `chat()` (lines 93-107, including the error-swallowing `if let Ok(text)` at line 99) all exist exactly as claimed, and the doc comment on `with_fallback` (lines 80-82) explicitly states the ingestion construction sites are "deliberately left unwired". Hidden-consumer search: a workspace-wide grep for `with_fallback`/`fallbacks` finds only the definition itself; the other hits are the unrelated `FailoverLLMProvider` (src/failover.rs), the scripted test adapter's `with_fallback_response`, and an orchestrator comment about content previews. Graphify shows LlmChatClient is consumed only by memory_llm/extraction.rs, memory_llm/merge.rs, and moa-memory/ingest (llm_extractor.rs:26, llm_merge.rs:39/49, contradiction.rs:265/417) — every construction site calls `from_api_key` (sometimes plus `with_rate_limits`/`with_endpoint`) and none chains a fallback, so `fallbacks` is always empty and the loop in `chat()` is provably unreachable. The feature is builder-only (not config/env driven), so no TOML, compose, or script could activate it; docs/ contain zero references to LlmChatClient fallbacks. No load-bearing constraint applies: this is a plain HTTP client, not Restate-replayed code, and AGENTS.md rule 7 explicitly forbids speculative compatibility scaffolding in this pre-prod repo. The wiremock tests in client.rs (lines 313-407) exercise chat/endpoint/pacer only and are unaffected; deleting the field, builder, and loop (making `chat` delegate to or absorb `chat_with_retry`) compiles cleanly against all callers. One rhetorical nit that does not change the verdict: FailoverLLMProvider covers the LLMProvider trait path, not memory ingestion, so it is a precedent for where failover lives rather than literal redundant coverage of the same path — but that does not make the dead chain load-bearing. Claimed side effects ("none") are accurate.

---

### 9. ProviderSelection stringly-typed round-trip over an existing typed API

**Area:** moa-providers
effort: **small** · finder confidence: **medium** · ~LOC removable: **~45**

**Locations**

- `crates/moa-providers/src/core/factory.rs:10-52`
- `crates/moa-providers/src/registry.rs:205-231`

**What it is.** resolve_provider_selection() calls the typed ProviderRegistry::resolve_selection_from_config() -> (ProviderId, ModelId), then converts both to Strings in a ProviderSelection struct; build_provider_from_selection() immediately re-parses provider_name back into a ProviderId via FromStr. build_provider_from_config() additionally constructs ProviderRegistry::from_config twice (once inside build_provider_from_selection, once for apply_main_failover).

**Why it may be over-engineered.** The String wrapper adds no invariant — it exists between two typed endpoints and forces a parse that can theoretically fail on data that was just produced typed. Consumers are only moa-eval/core/src/plan.rs and two moa-brain live tests, both of which could take (ProviderId, ModelId) directly (ProviderId::as_str() covers display needs).

**Simpler alternative.** Delete ProviderSelection; have resolve_provider_selection return (ProviderId, ModelId) (or just re-export resolve_selection_from_config), and have build_provider_from_selection accept those types. In build_provider_from_config, build the registry once and reuse it for both provider construction and apply_main_failover.

**Side effects / what to watch.** Small signature updates in moa-eval plan.rs and moa-brain live tests/example harness; no runtime behavior change.

**Value of simplifying.** Removes a lossy type round-trip, an impossible-error parse path, and a duplicate registry construction.

**Adversarial verifier: ✅ CONFIRMED.** Claim is factually accurate. crates/moa-providers/src/core/factory.rs:20-31 calls the typed ProviderRegistry::resolve_selection_from_config (registry.rs:206-231, returns (ProviderId, ModelId)) and stringifies both into ProviderSelection; factory.rs:48 re-parses provider_name with parse::<ProviderId>() — a fallible parse of a string just produced by ProviderId::as_str(), adding a dead error path. factory.rs:37-41 builds ProviderRegistry::from_config twice, and from_config (registry.rs:118-131) does a full Arc::new(config.clone()) of MoaConfig each time, so the duplication is real waste (no behavior bug: apply_main_failover only builds fallbacks). Consumer audit (workspace grep + graphify): only moa-eval/core/src/plan.rs:55-58 (passes the selection straight back into build_provider_from_selection — a typed tuple works directly) and moa-brain/tests/brain_turn_live.rs:40-42 (assigns into config String fields; as_str().to_string() covers it); cache_audit_live.rs and examples/chat_harness.rs only call build_provider_from_config, whose signature is unchanged. No hits in scripts, compose, docs, or config. No load-bearing constraint: ProviderSelection derives only Debug/Clone/PartialEq/Eq — no serde, so it never crosses a Restate journal/wire boundary and is absent from moa-orchestrator entirely; no replay/perf/security doc forces it, and AGENTS.md rule 7 (no shims to preserve paths) plus the pre-prod no-compat stance favor deletion. apply_main_failover is pub(crate) and takes &self, so building the registry once in build_provider_from_config and reusing it is behavior-identical. Side effects the claimant missed are trivial: the re-export list in crates/moa-providers/src/lib.rs:25-26 and the five factory.rs unit tests asserting on selection.provider_name/model_id (become provider_id.as_str() comparisons); conversely the example harness and cache_audit_live.rs listed by the claimant need no changes at all.

---

### 10. Parser-webhook stack (routes, dual signature verification, config knobs) for an async parse flow that does not exist

**Area:** moa-knowledge
effort: **medium** · finder confidence: **high** · ~LOC removable: **~700**

**Locations**

- `crates/moa-knowledge/src/parser/mod.rs:44-377`
- `crates/moa-edge/src/routes/webhook_verification.rs:32-76`
- `crates/moa-edge/src/routes/webhook_verification.rs:100-132`
- `crates/moa-orchestrator/src/services/knowledge/webhook_verifier.rs`
- `crates/moa-orchestrator/src/services/knowledge/webhook.rs:353-369`
- `crates/moa-core/src/config/knowledge.rs:186-198,253-269`
- `crates/moa-core/src/config/env_overlay.rs:237-268`
- `crates/moa-knowledge/tests/knowledge_offline/parser_llamaparse.rs:149-165`
- `crates/moa-knowledge/tests/knowledge_offline/parser_reducto.rs:245-260`

**What it is.** There is a full inbound-webhook pipeline for the document parsers: edge routes /v1/knowledge/webhooks/llamaparse and /reducto, edge-side HMAC + Svix signature verification with replay-window checks (webhook_verification.rs), a second orchestrator-side verification via verify_parser_webhook/map_parser_webhook in moa-knowledge (Svix key decode, 3 base64 variants, hex fallback, custom-header knobs, and a metadata mapper that probes ~10 candidate JSON paths per field), plus per-parser config knobs (webhook_signing_key, webhook_header_name, webhook_header_value for both LlamaParse and Reducto) and env overlays.

**Why it may be over-engineered.** No webhook can ever meaningfully arrive. All three external parsers parse synchronously by polling (llamaparse.rs POLL_ATTEMPTS/POLL_INTERVAL, reducto.rs same; no parse submission ever registers a callback URL with the vendor), and even if a webhook did arrive, should_enqueue_ingestion in webhook.rs:353-360 returns false for llamaparse/reducto/unstructured, so a fully verified parser webhook only inserts a knowledge_provider_events row and does nothing else. This is two layers of cryptographic verification plus routes, config, and tests guarding a dead-letter box.

**Simpler alternative.** Delete the parser-webhook surface entirely until an async parse-callback flow exists: remove lines 44-377 of crates/moa-knowledge/src/parser/mod.rs (keep only the DocumentParser trait and is_external_document_parser), the llamaparse/reducto branches in the edge verifier, the ParserWebhookVerifier, the is_parser_origin_provider carve-out in webhook.rs, the webhook_* config fields and their env overlays, and the offline webhook tests. Parsing already works end-to-end via polling.

**Side effects / what to watch.** The two edge routes disappear (docs/21-tenant-knowledge-base.md route list needs updating); loses the audit-trail row for hypothetical parser webhooks; if a webhook-driven parse-completion flow is built later it must be re-added, but then with an actual consumer. Offline tests for webhook verification are deleted; no production flow changes.

**Value of simplifying.** Removes ~700 lines including a security-sensitive unauthenticated ingress path that is verified twice but drives nothing, two config knob clusters that are never set, and duplicate Svix/HMAC implementations in edge and moa-knowledge.

**Adversarial verifier: ✅ CONFIRMED.** Every factual element of the claim checks out, and the dead-letter argument is actually STRONGER than stated.

1. Factual accuracy — confirmed line by line. crates/moa-knowledge/src/parser/mod.rs:44-377 is entirely webhook verification/mapping (verify_parser_webhook, Svix path with 300s replay window, 3 base64 variants + hex fallback in decode_base64_signature/decode_signature, whsec_ key decode, and parser_webhook_metadata probing ~8-10 candidate JSON paths per field); the DocumentParser trait and is_external_document_parser (lines 24-42) are outside the deletion range as the claim says. The edge duplicates the same crypto independently (crates/moa-edge/src/routes/webhook_verification.rs:32-45 llamaparse/reducto branches, verify_svix_signature_at_edge:100-132), and the orchestrator re-verifies a second time via ParserWebhookVerifier (crates/moa-orchestrator/src/services/knowledge/webhook_verifier.rs:123-137) built from config in services/knowledge/mod.rs:828-874. Config knobs and env overlays exist exactly as cited (config/knowledge.rs LlamaParse/Reducto webhook_signing_key/header_name/header_value; env_overlay.rs MOA_*_WEBHOOK_* fields).

2. The parsers really are synchronous pollers. llamaparse.rs:15-16,138-156 and reducto.rs:15-16,136-154 use POLL_ATTEMPTS=30/POLL_INTERVAL=2s; grep for "webhook"/"callback" across all three parser adapter files returns nothing — no parse submission ever registers a callback URL. Reducto's async_enabled path posts only {input, options, settings} then polls /job/{job_id} (reducto.rs:80-180); that knob is polling-async, not webhook-related, and the proposal correctly leaves it alone.

3. The flow is even deader than claimed. should_enqueue_ingestion (webhook.rs:353-361) returns false for all parser providers, so a verified webhook only records a knowledge_provider_events row. But additionally, resolve_verified_webhook_binding (webhook.rs:161-217) requires tenant_id+connection_uid in the payload metadata OR a provider_account_binding_candidate — which returns None for anything but nango/merge (webhook.rs:298-321). Since no parse submission attaches tenant/connection metadata for the vendor to echo back, a REAL vendor-originated webhook (e.g., dashboard-configured) could never bind and would error with InvalidRequest before recording anything. The "lost audit-trail row" side effect is therefore reachable only by synthetic test payloads. The orchestrator's own test pins this deadness: knowledge_service.rs asserts !response.ingestion_enqueued, sync_run_count()==0, step_count()==0 after a fully verified parser webhook. Also, "unstructured" in is_parser_origin_provider is doubly dead — webhook_verifier resolution in mod.rs only matches llamaparse/reducto, so an unstructured webhook is UnknownProvider anyway.

4. No hidden consumers. verify_parser_webhook/map_parser_webhook are used only by ParserWebhookVerifier and the two offline tests (the edge has its own parallel implementation). is_external_document_parser has a real consumer (ingestion.rs:1720) and is explicitly kept. is_parser_origin_provider is used only inside webhook.rs. No scripts, compose files, or e2e tests hit /v1/knowledge/webhooks/llamaparse|reducto. The WebhookEvent domain type and nested_value are shared with nango/merge and stay.

5. No load-bearing constraint forces this. Nothing Restate-replay-related (deleting a service entry path is fine pre-prod, no backwards compat per repo policy); docs/08-security has no mandate to keep dead unauthenticated ingress routes — deleting two unauthenticated POST endpoints reduces attack surface; nango/merge webhook verification (the live flow) is untouched.

Side effects the claimant missed (additive, none change the verdict):
- crates/moa-edge/src/routes/knowledge.rs tests: providers array at line 326 includes "llamaparse"/"reducto", and the raw-document rejection test at line 419 calls translate_provider_webhook("llamaparse", ...) — both need trimming to nango/merge.
- crates/moa-orchestrator/tests/knowledge_service.rs:~592-720: two parser-webhook integration tests plus the ParserWebhookVerifier import (line 58) must be deleted.
- .env.example lines 62 and 65 (MOA_LLAMAPARSE_WEBHOOK_SIGNING_KEY, MOA_REDUCTO_WEBHOOK_SIGNING_KEY).
- ParserWebhookVerifier is pub-re-exported at crates/moa-orchestrator/src/services/knowledge/mod.rs:11.
- At the edge, verify_svix_signature_at_edge and svix_signing_key (webhook_verification.rs:100-132,172-177) are used ONLY by the parser branch (nango/merge use plain HMAC headers), so they and their unit test become dead code to remove too.
- connection_provider_matches_webhook (webhook.rs:367-369) collapses to plain equality.
- The subtle/ConstantTimeEq + hex deps in the orchestrator verifier and possibly hmac/sha2/base64 in moa-knowledge may become prunable.

---

### 11. KnowledgeRepository default method bodies silently replace concurrency-critical semantics with weaker in-memory versions

**Area:** moa-knowledge
effort: **small** · finder confidence: **high** · ~LOC removable: **~140**

**Locations**

- `crates/moa-knowledge/src/repository.rs:117-120`
- `crates/moa-knowledge/src/repository.rs:181-234`
- `crates/moa-knowledge/src/repository.rs:256-290`
- `crates/moa-knowledge/src/repository.rs:1200-1222`
- `crates/moa-orchestrator/tests/knowledge_service.rs:4132`

**What it is.** The KnowledgeRepository trait gives default bodies to its concurrency primitives: claim_sync_run defaults to a plain non-atomic insert, claim_document_version_ingestion defaults to insert-then-always-Claimed with a fabricated token (no fencing), complete/fail_document_version_ingestion default to no-ops, and unseen_active_objects_for_connection defaults to loading every active object into memory and filtering client-side (which is why the otherwise-unused trait method active_objects_for_connection and its Postgres impl exist at all). The only type relying on these defaults is the InMemoryKnowledgeRepository test fake in moa-orchestrator/tests/knowledge_service.rs, which does not override them.

**Why it may be over-engineered.** This is a near-duplicate parallel implementation of the crate's hardest invariants (sync-run single-claim, version-ingestion fencing, keyset prune pagination), maintained inside the trait purely so one test fake compiles. Any future second implementation silently inherits non-atomic claims and no-op completion, and tests running through the defaults are not exercising the fencing behavior the pipeline depends on.

**Simpler alternative.** Make claim_sync_run, claim_document_version_ingestion, complete/fail_document_version_ingestion, and unseen_active_objects_for_connection required trait methods; implement honest in-memory versions in the InMemoryKnowledgeRepository fake; delete active_objects_for_connection (trait method + Postgres impl) entirely since its only caller is the default body being removed.

**Side effects / what to watch.** The test fake needs ~100 lines of real claim/prune logic (or the affected orchestrator tests move to the Postgres-backed lane); net LOC reduction is modest but a whole class of silent-weak-semantics bugs disappears. No production behavior changes.

**Value of simplifying.** Removes a second, weaker implementation of the fencing/claim logic and a dead-in-production repository method plus its SQL; makes the trait contract honest.

**Adversarial verifier: ✅ CONFIRMED.** Every factual assertion checks out against the code. (1) The defaults exist exactly as claimed in /Users/hwuiwon/Github/moa/crates/moa-knowledge/src/repository.rs: claim_sync_run (L117-120) delegates to plain create_sync_run and unconditionally returns SyncRunClaim::Claimed, so the AlreadyRunning variant (L60) is unreachable through the default; claim_document_version_ingestion (L256-268) fabricates a claim_token with Uuid::now_v7(), discards sync_run_uid, and always returns Claimed, so AlreadyInProgress/AlreadyCompleted (L74-76) are unreachable; complete/fail_document_version_ingestion (L271-290) are pure no-ops that discard the fencing token; unseen_active_objects_for_connection (L198-234) loads all active objects and filters/sorts/paginates client-side. (2) Exactly two implementors exist in the whole workspace: PostgresKnowledgeRepository (repository.rs:443), which overrides all five with real atomic semantics (partial-unique ON CONFLICT claim at L657-748, claims table with lease + fencing token at L1377+, token-checked complete/fail at L1524/L1557, SQL keyset pagination at L1224), and the test fake InMemoryKnowledgeRepository (/Users/hwuiwon/Github/moa/crates/moa-orchestrator/tests/knowledge_service.rs:4132), which overrides none of the five — verified by enumerating all its async fns (L4133-4592). (3) active_objects_for_connection's only caller anywhere is the default body (repository.rs:211); the Postgres impl (L1200-1223) and the fake impl (L4431) exist solely to satisfy it, so deleting it is safe — no hits in docs/, scripts/, or config. (4) No load-bearing constraint forces the defaults: docs never mention KnowledgeRepository or these methods; Restate determinism is unaffected (trait shape does not change durable-step behavior); the performance-relevant SQL impls are kept; AGENTS.md rule 7 (no compatibility shims) and the test value bar (exercise real production paths) actually favor the removal; the repo is pre-prod with no compat requirements. (5) The concrete hazard is real: the orchestrator service's claim path (sync.rs:73-78, webhook.rs:133-142) matches on SyncRunClaim, and through the fake the AlreadyRunning branch can never fire — grep confirms zero orchestrator-test coverage of AlreadyRunning/dedupe. Real fencing is covered only in the Postgres db_memory lane (crates/moa-knowledge/tests/knowledge_db_memory/sync_run_db_memory.rs, ingestion_pipeline_db_memory.rs), so making the methods required loses no production coverage. One refinement to the claimant's side-effect estimate: orchestrator tests build KnowledgeService with fake_ingestion_runner()/FakeKnowledgeSyncIngestionSteps (knowledge_service.rs:2213-2245), so the fake never actually reaches claim_document_version_ingestion, complete/fail, or unseen_active_objects_for_connection at runtime — only claim_sync_run is live through the fake. The fake therefore needs a real in-memory single-claim for claim_sync_run (~20 lines checking active statuses) plus simple honest bodies (or explicit erroring stubs) for the other four; "~100 lines" is an overestimate, and this makes the simplification cheaper than claimed, not more expensive.

---

### 12. Dead code and dead config knobs left behind by refactors

**Area:** moa-knowledge
effort: **small** · finder confidence: **high** · ~LOC removable: **~230**

**Locations**

- `crates/moa-knowledge/src/ingestion.rs:474-531`
- `crates/moa-knowledge/src/ingestion.rs:528,640,656`
- `crates/moa-knowledge/src/repository.rs:299,1730-1755`
- `crates/moa-knowledge/src/normalize.rs:122-150`
- `crates/moa-knowledge/src/graph_delta.rs:18-21,99`
- `crates/moa-core/src/config/knowledge.rs:332-349,477-480`

**What it is.** A cluster of unreferenced production surface: (1) KnowledgeIngestionPipeline::ingest_parsed_object has zero callers anywhere in the workspace; (2) the persist_parsed deleted_chunk_uids parameter is Vec::new() at both call sites, so the plumbing that chains it into tombstones is dead; (3) KnowledgeRepository::set_chunk_graph_uid (trait method + 26-line Postgres UPDATE) survives only as a comment and a test asserting it is called zero times — the batch replace_chunks path replaced it; (4) normalize_provider_record duplicates the pipeline's materialize_object and has zero callers; (5) KnowledgeGraphDelta.tombstone_chunk_hashes is always empty and never read; (6) config knobs observability.store_step_rows and observability.query_trace_enabled are never read by any production code (step rows are unconditionally persisted; the query-trace route ignores the flag).

**Why it may be over-engineered.** Pure leftovers: each item is either the old half of a completed refactor or a knob wired through config/env overlay with no consumer. MOA is pre-production, so none of it is needed for compatibility.

**Simpler alternative.** Delete ingest_parsed_object, the deleted_chunk_uids parameter, set_chunk_graph_uid (trait method, Postgres impl, fake impl, and the op_count assertion), normalize_provider_record, the tombstone_chunk_hashes field, and the store_step_rows/query_trace_enabled config fields with their env-overlay lines (or wire query_trace_enabled to actually gate the trace route if gating is wanted).

**Side effects / what to watch.** None observable: no production caller exists for any of it. One orchestrator test assertion (set_chunk_graph_uid op_count == 0) and one env-overlay test line are removed alongside.

**Value of simplifying.** ~230 lines deleted, one fewer trait method every future repository impl must stub, and a config file that only lists knobs that do something.

**Adversarial verifier: ✅ CONFIRMED.** Every item verified dead by workspace-wide grep (all file types, including tests, scripts, TOML/YAML/env/compose, docs) plus graphify orientation.

(1) ingest_parsed_object (crates/moa-knowledge/src/ingestion.rs:474-531): the only occurrence in the entire repo is its definition. Zero production, test, or doc callers.

(2) deleted_chunk_uids: both persist_parsed call sites (ingestion.rs:528 and 640) pass Vec::new(). The param flows persist_parsed -> persist_claimed_version (line 770/792), where it only pads a counter at line 864 (always +0) and is chained into tombstones at line 1025 (always empty). Removing it leaves the live orphan_chunks tombstoning intact; nothing else calls persist_parsed/persist_claimed_version.

(3) set_chunk_graph_uid: trait method (repository.rs:299), Postgres impl (repository.rs:1730-1755), fake impl (moa-orchestrator/tests/knowledge_service.rs:4538-4543), and one op_count==0 assertion (knowledge_service.rs:110, inside a test where nearly all repo ops are asserted 0 because nothing ingests). No caller anywhere. Comments at ingestion.rs:831 and :1542 explicitly describe it as the "former"/"previous" per-chunk write that batch replace_chunks replaced — the code itself confirms the refactor completed.

(4) normalize_provider_record (normalize.rs:124-150): only occurrence is the definition. It near-duplicates the live private materialize_object (ingestion.rs:1220-1248); the differences (seed format, Pending vs Active status) are moot since nothing calls it. The neighboring functions in normalize.rs (normalize_source_selection, redact_provider_metadata) are live and untouched by the proposal.

(5) tombstone_chunk_hashes (graph_delta.rs:20, 99): only ever initialized to Vec::new(), never pushed to, never read. It carries #[serde(default)], and no snapshot/fixture/non-Rust file references the field, so removal is serialization-safe even against any stored payloads — and MOA is pre-prod with an explicit no-backwards-compat policy.

(6) Config knobs (moa-core/src/config/knowledge.rs:332-349, 477-480): store_step_rows has no env overlay, no TOML/compose consumer, and no production read — record_step calls in ingestion.rs persist step rows unconditionally. query_trace_enabled has an env overlay (MOA_KNOWLEDGE_QUERY_TRACE_ENABLED, env_overlay.rs:268-269, apply at knowledge.rs:477-480, test at env_overlay.rs:1074/1144) but the query_trace handler (moa-orchestrator/src/services/knowledge/inspect.rs:168-178) gates only on postgres-pool presence and never reads the flag — exactly as claimed. The third field in the same struct, max_object_preview_chars, IS live (services/knowledge/mod.rs:920), and the claim correctly targets only the two dead fields, not the struct.

Load-bearing constraints: none apply. Dead code participates in no Restate journal replay; no doc, security (redaction paths are live and untouched), or test-lane requirement depends on any of it; AGENTS.md rule 7 and the repo's no-backwards-compat stance actively favor deletion.

Two trivial extras the claimant did not list, neither of which is breakage: (a) docs/10-technology-stack.md:128 mentions "query trace enablement" in the MOA_KNOWLEDGE_* env-var row and should be trimmed when the env overlay is removed; (b) the historical comments at ingestion.rs:831 and :1542 that name set_chunk_graph_uid should be reworded so they do not reference a deleted symbol. Both are one-line cosmetic follow-ups that do not change the verdict.

---

### 13. IngestionObserver trait has exactly one implementation workspace-wide and exists only to add a fifth generic parameter to the pipeline

**Area:** moa-knowledge
effort: **small** · finder confidence: **high** · ~LOC removable: **~60**

**Locations**

- `crates/moa-knowledge/src/observability.rs:190-256`
- `crates/moa-knowledge/src/ingestion.rs:240-250,263-292,1465-1467`

**What it is.** IngestionObserver is an async trait with one method (record_step). Its only implementation anywhere — including every test file — is MetricsIngestionObserver, a zero-sized struct that records tracing fields and emits metrics. The pipeline struct KnowledgeIngestionPipeline<R, P, E, G, O> carries it as generic parameter O and an Arc<O> field, and every construction site (orchestrator ingest.rs and all five db_memory test setups) passes Arc::new(MetricsIngestionObserver).

**Why it may be over-engineered.** A trait with a single impl and not even a test double is an abstraction seam nobody uses. R, P, E, G all have second implementations (Postgres/fakes, four parsers, fake graph writers); O never varies. The async dispatch, Arc, and generic parameter buy nothing over a direct function call.

**Simpler alternative.** Delete the trait and MetricsIngestionObserver; move its body into a plain fn record_step_metrics(labels, &outcome) in observability.rs and call it directly inside KnowledgeIngestionPipeline::record_step_with_counters. The pipeline drops to four generic parameters and loses the observer field and constructor argument.

**Side effects / what to watch.** Constructor signature change touches the orchestrator ingest.rs wiring and the db_memory test setups (mechanical, one argument removed each). If someone later wants to capture steps in tests, the persisted knowledge_ingestion_steps rows already provide that.

**Value of simplifying.** ~60 lines deleted, one less type parameter on the crate's central struct, one less async-dyn hop per ingestion step (steps are recorded ~10 times per object).

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy: verified. crates/moa-knowledge/src/observability.rs:191-201 defines IngestionObserver as an #[async_trait] with the single method record_step returning Result<()>; lines 204-248 define MetricsIngestionObserver, a zero-sized Copy struct whose impl only records tracing span fields, a histogram, counter metrics, and one tracing::info!. It always returns Ok(()). crates/moa-knowledge/src/ingestion.rs:240-250 carries O as the fifth generic parameter with an Arc<O> field; lines 263-292 bound O: IngestionObserver and take Arc<O> in the constructor; lines 1465-1467 are the sole call site, inside record_step_with_counters, which needs outcome.clone() only because the trait takes StepOutcome by value.

Hidden consumers: a workspace-wide grep (crates/, docs/, scripts/, compose/config files) found only: the defining module, ingestion.rs, crates/moa-orchestrator/src/services/knowledge/ingest.rs (type alias ProductionKnowledgeIngestionPipeline at ~line 133 with MetricsIngestionObserver as fifth arg, and Arc::new(MetricsIngestionObserver) at line 174), and tests. Every single construction site — production and all tests — passes Arc::new(MetricsIngestionObserver). There is no test double, no second impl, no doc mentioning IngestionObserver (docs grep hits only unrelated "observers" in docs/17-observability.md), and it is not in the docs/01 trait map.

Load-bearing constraints: none apply. Metrics/tracing are fire-and-forget side effects outside Restate journal determinism; the durable record is the knowledge_ingestion_steps row persisted right after via build_step_row + repository.record_ingestion_step[_once] (ingestion.rs:1468-1478), which is untouched. No feature flag gates the observer. AGENTS.md rule 7 (no abstraction shims to preserve paths) actively favors removal. Bonus: record_step_with_counters (lines 1455-1458) already records span "status" and "error_code" fields, which MetricsIngestionObserver::record_step duplicates (observability.rs:222-226) — inlining removes that duplication. The trait's Result return is a dead error path (the only impl never errs), and inlining as an infallible plain fn taking &StepOutcome also eliminates the outcome.clone() and the async dispatch for purely synchronous work.

Side effects: the claimant slightly undercounted mechanical touch points but missed nothing structural. Actual sites to update: (a) orchestrator ingest.rs — constructor call plus the ProductionKnowledgeIngestionPipeline type alias losing its fifth type arg; (b) crates/moa-orchestrator/tests/knowledge_service.rs — 2 construction sites (lines ~1255, ~2751); (c) crates/moa-knowledge/tests/knowledge_db_memory/ingestion_pipeline_db_memory.rs — ~12 sites; (d) crates/moa-knowledge/tests/knowledge_db_memory/observability_db_memory.rs — 1 site; plus possibly an unused async_trait import in observability.rs. All are one-argument/one-type-arg deletions. The proposed fn record_step_metrics(labels, &outcome) called inside record_step_with_counters is strictly simpler and behavior-preserving; the persisted steps table already covers any future test capture need.

---

### 14. normalization.rs keeps a shape-based fallback path that duplicates the descriptor-driven review/pattern logic, including dead tool-name string matches

**Area:** moa-hands (tools/sandboxes/MCP)
effort: **small** · finder confidence: **medium** · ~LOC removable: **~90**

**Locations**

- `crates/moa-hands/src/core/normalization.rs:157-246`
- `crates/moa-hands/src/core/normalization.rs:248-337`
- `crates/moa-hands/src/core/normalization.rs:112-132`
- `crates/moa-hands/src/tools/sandbox_descriptor.rs:64-101`

**What it is.** review_fields_for first resolves a SandboxToolDescriptor and dispatches on descriptor.review_preview (sandbox_review_fields_for, ~90 LOC); when no descriptor matches it falls through to a shape-based match that re-implements the same rendering, including hardcoded `invocation.name == "file_write"` and `== "str_replace"` branches with the identical char-count field logic. action_pattern_for has the same two-layer structure (descriptor strategy vs action_pattern_for_shape). The descriptor metadata itself parameterizes constant strings (command_field: "cmd", command_label: "Command", working_dir_label: "Working dir") that never vary.

**Why it may be over-engineered.** The name-specific fallback branches are unreachable: 'file_write', 'str_replace', and 'bash' (the only Command-shaped tool) always resolve a descriptor, so the fallback only ever serves built-in/MCC/procedure tools whose shapes are Json or Pattern. Maintaining two renderings of the same preview means any change to how a file-write review looks must be made twice, and the SandboxReviewPreviewMetadata field/label parameterization adds indirection for values that are identical across all seven descriptors.

**Simpler alternative.** Delete the Command arm and the file_write/str_replace name checks from the shape fallback (keep only the trivial single-field/Json arms for descriptor-less tools), or make the fallback shape-arms delegate to the same rendering helpers the descriptor path uses. Drop the constant command_field/command_label/working_dir_label parameters from SandboxReviewPreviewMetadata::Command since every user passes the same literals.

**Side effects / what to watch.** None observable: the deleted branches are unreachable for the tools they name. A hypothetical future register_hand tool with Command/Path shape and no descriptor would get the generic single-field preview instead of the rich one — acceptable and easily restored via a descriptor.

**Value of simplifying.** ~90 LOC removed and one source of truth for admin-review previews, so preview changes cannot drift between the descriptor and fallback paths.

**Adversarial verifier: ✅ CONFIRMED.** Every factual assertion checks out against the code. (1) crates/moa-hands/src/core/normalization.rs:162 resolves sandbox_tool_descriptor(&invocation.name) and returns early, and the static table in crates/moa-hands/src/tools/sandbox_descriptor.rs:217-328 always matches names "file_write", "str_replace", and "bash"; therefore the fallback's `invocation.name == "file_write"` (line 188) and `== "str_replace"` (line 200) branches are dead, and they duplicate the identical char-count field rendering in sandbox_review_fields_for (lines 280-335). (2) A whole-workspace grep for ToolInputShape construction outside the two cited files finds only Json (MCP tools in core/registration.rs:87, procedure tools in core/policy.rs:306, memory.rs:14, lifecycle.rs:1030, orchestrator tool_executor.rs:1079, tool_result.rs:67), Query (session_search.rs:49), and Pattern (tool_result.rs:222) — no descriptor-less tool is Command- or Path-shaped, so the fallback Command arm (lines 167-185) is unreachable too, confirming bash is the only Command-shaped tool. (3) SandboxReviewPreviewMetadata::Command has exactly one user (the bash descriptor, sandbox_descriptor.rs:229-233) passing the constant literals "cmd"/"Command"/"Working dir", so those parameters are pure indirection; the SingleField field/label params DO vary across descriptors and are justified, and the claim correctly does not target them. (4) Hidden consumers: the only test asserting "Working dir" (crates/moa-hands/tests/hands_offline/local_tools_offline.rs:408-440) invokes tool name "bash" through the descriptor path and is unaffected. No load-bearing constraint applies: the descriptor path emits byte-identical review previews before and after, so Restate replay/persisted ActionEnvelope content is unchanged, and the repo is pre-prod with no back-compat requirement. Two minor nuances that do not change the verdict: the match over ToolInputShape must stay exhaustive, so "delete the Command arm" means collapsing it to a generic single-field arm (which the claim's parenthetical already allows); and action_pattern_for_shape is only 6 lines delegating to the shared shell_action_pattern_for helper (no duplicated rendering there), with unit tests at normalization.rs:499-546 calling it directly with Command shape — removing that arm just means retargeting those tests at shell_action_pattern_for. The claimed side effect (a hypothetical future descriptor-less Command/Path tool gets a generic preview) is accurate and acceptable.

---

### 15. Dead constructor and config surface on PostgresSessionStore and the blob backend enum

**Area:** session / db / migrations / runtime-store
effort: **small** · finder confidence: **high** · ~LOC removable: **~90**

**Locations**

- `crates/moa-session/src/store/mod.rs:111-123`
- `crates/moa-session/src/store/mod.rs:138-149`
- `crates/moa-session/src/store/mod.rs:193-211`
- `crates/moa-session/src/blob.rs:216-219`
- `crates/moa-core/src/config/session.rs:9-30`

**What it is.** PostgresSessionStore exposes seven public constructors. from_admin_config has zero callers anywhere in the workspace; new(database_url) is called only by one error-path db test and duplicates a hard-coded default-backends wiring (FileBlobStore + local rustfs, threshold 65536) that also exists in from_existing_pool and isolated_test_backends; from_existing_pool is used only by the ignored Neon live test. Separately, SessionBlobBackend::ObjectStore is a config enum variant whose only behavior is returning a 'not implemented; use postgres' ConfigError from blob_store_from_config.

**Why it may be over-engineered.** Constructors and config variants that exist for callers that never materialized are pure surface area: three near-duplicate wiring paths must be kept consistent (the 65536 default is repeated three times), and an unimplemented enum variant is speculative extensibility that a future implementer would rewrite anyway. Pre-production, nothing external depends on these entry points.

**Simpler alternative.** Delete from_admin_config and from_existing_pool outright; delete new() and point its single test at from_config or new_in_existing_schema; extract the one shared default-backends helper. Drop the ObjectStore variant from SessionBlobBackend (re-add when actually implemented) so config validation is just Local-vs-Postgres.

**Side effects / what to watch.** One db test and the Neon live test need small rewrites (the latter disappears entirely if the Neon finding is applied); admin_url()-based store construction would need re-adding if an admin-URL path is ever wanted.

**Value of simplifying.** Removes ~90 lines and three divergence-prone duplicate wiring paths from the crate's most safety-critical type, and one config value that can only ever produce an error.

**Adversarial verifier: ✅ CONFIRMED.** Every factual assertion verified against the code. (1) Constructor count: PostgresSessionStore in crates/moa-session/src/store/mod.rs exposes exactly seven public constructors (new L113, from_config L126, from_admin_config L139, new_in_schema L155, new_in_existing_schema L174, from_existing_pool L197, from_existing_pool_with_config L214). (2) Dead callers: workspace-wide grep (crates/, tests/, docs/, scripts/, configs) finds zero callers of from_admin_config — only its definition. PostgresSessionStore::new has exactly one caller: the ignored error-path test postgres_connection_retry_surfaces_final_failure at crates/moa-session/tests/postgres_store_db.rs:1290, which only exercises connect_with_retry ("after 3 attempts") and never reaches blob/attachment wiring, so pointing it at from_config (bad database.url) or new_in_existing_schema hits the identical shared retry path — the retry logic lives in connect_with_retry (mod.rs:449), used by all constructors. from_existing_pool's only caller is the ignored Neon live test (crates/moa-session/tests/neon_branch_manager_live.rs:171); production callers (crates/moa-edge/src/main.rs:73, crates/moa-orchestrator/src/runtime/deps.rs:94) all use the separate from_existing_pool_with_config, which stays. (3) Duplication: the 65_536 threshold + FileBlobStore + local_rustfs_config() attachment wiring is repeated verbatim in new() (L114-121), from_existing_pool (L198-207), and isolated_test_backends (L615-624), exactly as claimed. (4) ObjectStore variant: its only behavior anywhere is the ConfigError at crates/moa-session/src/blob.rs:216-218; no config file, compose file, script, doc, env sample, or test sets blob_backend=object_store, and no test pins the error message. The unrelated `object_store` crate usages (moa-session/src/attachment_storage.rs, moa-lineage/audit/src/merkle.rs) are the attachments/audit subsystems and are untouched by dropping the SessionBlobBackend variant. (5) No load-bearing constraint: these are startup wiring paths, not Restate durable-execution surface; docs/02/18/08 impose nothing on constructor count, and AGENTS.md rule 7 ("no compatibility shims... to preserve old paths") plus the pre-prod no-backwards-compat stance actively support the deletion. (6) Bonus footgun the claim implies: new() and from_existing_pool silently hard-wire attachments to the local RustFS dev endpoint with dev credentials (local_rustfs_config), so any future production caller of these convenience constructors would misroute attachments — deleting them removes that trap. Minor side-effect additions the claimant missed, none blocking: (a) with the variant removed, MOA_SESSION_BLOB_BACKEND=object_store fails as a serde unknown-variant parse error (which still lists the valid backends) instead of the curated ConfigError; (b) DatabaseConfig::admin_url() itself keeps its real callers (orchestrator main.rs:96, runtime/database.rs:50 for migrations), so only the unused constructor disappears, not admin-URL capability; (c) the Neon test rewrite to from_existing_pool_with_config needs a MoaConfig with session.attachments = local_rustfs (or equivalent) since it now goes through AttachmentObjectStore::from_config, and it gains the background gauge refresher spawn — both harmless in a live test. These are refinements, not corrections; the proposed simplification is safe and strictly reduces surface.

---

### 16. Citation 'NLI cascade' is speculative machinery around a model that does not exist

**Area:** lineage / ocsf / observability
effort: **medium** · finder confidence: **high** · ~LOC removable: **~200**

**Locations**

- `crates/moa-lineage/citation/src/verifiers.rs:22-26,72-129 (CitationVerifier trait, NliVerifier facade)`
- `crates/moa-lineage/citation/src/cascade.rs:17-38 (CascadeConfig incl. never-read max_concurrent_nli),49-58 (bm25_only),256-299 (dead emit_verifier_scores)`
- `crates/moa-brain/src/lineage.rs:300-308 (the single production constructor)`

**What it is.** The citation verifier is built as a pluggable two-stage cascade: a `CitationVerifier` async trait with three impls (Bm25Verifier, NliVerifier, CascadeVerifier), an `Option<NliVerifier>` second stage, a `CascadeConfig` with four knobs, and a `bm25_only()` constructor. `NliVerifier` is explicitly a 'facade for a future NLI-backed stage' that today computes token-set overlap plus a five-word negation heuristic.

**Why it may be over-engineered.** Production has exactly one configuration: moa-brain always builds `CascadeVerifier::new(.., Some(NliVerifier::new("lexical-overlap-fallback")))`, so the Option, the trait polymorphism, and `bm25_only()` (used only in the crate's own tests) never vary. No caller uses `dyn CitationVerifier` or a generic bound. `CascadeConfig.max_concurrent_nli` is 'reserved for the ONNX-backed verifier' and is never read anywhere. `emit_verifier_scores` (cascade.rs:257) is exported but has zero callers — moa-brain has its own near-identical `emit_citation_scores`.

**Simpler alternative.** Collapse to one concrete `CascadeVerifier` struct with plain functions: `score_bm25` shortlist then the lexical-overlap scorer inline (keeping the honest 'lexical_overlap' method label). Delete the `CitationVerifier` trait, the `NliVerifier` type, the `nli: Option<_>` field, `max_concurrent_nli`, `bm25_only()`, and `emit_verifier_scores`. Reintroduce a trait if and when a real ONNX NLI runtime actually lands.

**Side effects / what to watch.** crates/moa-lineage/citation/tests/adapters.rs tests using bm25_only/CitationVerifier need small rewrites; moa-brain's constructor call shrinks. The recorded `method` strings ('bm25+lexical_overlap') should be preserved so lineage rows stay truthful.

**Value of simplifying.** Deletes an async-trait vtable layer and a fake-model facade from a per-sentence hot loop, removes a dead config knob and a dead public function, and makes the actual verification algorithm readable in one place.

**Adversarial verifier: ✅ CONFIRMED.** Every factual assertion verified against the code. (1) crates/moa-lineage/citation/src/verifiers.rs:22-26 defines the async CitationVerifier trait with exactly three impls (Bm25Verifier:51, NliVerifier:101, CascadeVerifier in cascade.rs:158); NliVerifier's doc comment (verifiers.rs:72-78) explicitly says it is a facade for a future HHEM/ONNX NLI runtime, and its body is token-set overlap plus a five-word negation list (verifiers.rs:200-208: not/never/no/none/without). (2) Workspace-wide grep (crates/, docs/, *.toml/yml/env, scripts, compose): `max_concurrent_nli` appears only at its declaration and Default (cascade.rs:27,36) — never read anywhere, matching its own 'Reserved for the ONNX-backed verifier' comment. `emit_verifier_scores` (cascade.rs:257) is pub-exported from lib.rs:16 but has zero callers in the entire repo; moa-brain has its own near-duplicate `emit_citation_scores` (crates/moa-brain/src/lineage.rs:310) which is the one actually used (and it additionally records durably via LineageHandle and sets user_id, so the dead export is not even the better version). (3) The only production constructor is crates/moa-brain/src/lineage.rs:300-308, always passing `Some(NliVerifier::new("lexical-overlap-fallback"))`; the model_name is never surfaced anywhere (the recorded method strings are the constants "lexical_overlap"/"bm25+lexical_overlap"). `bm25_only()` is used only by the crate's own tests (crates/moa-lineage/citation/tests/adapters.rs:321,385,442). No `dyn CitationVerifier` or generic bound exists anywhere; the only other consumers of the crate are moa-orchestrator (brain_bridge.rs:15, turn_execution.rs:53) importing only ChunkRef. (4) No load-bearing constraint: the verifier is pure deterministic computation, so Restate replay determinism (docs/02) is unaffected by collapsing it; docs contain no mandate for an NLI cascade (grep for nli/hhem/bm25/cascade across docs/ hits only unrelated content); no config/TOML/compose wiring references these knobs. (5) Extra evidence beyond the claim: production always calls verify_all with `citations: &[]` (lineage.rs:269), so the vendor-citation re-verification path and the nli-is-None branch (cascade.rs:187-203, `verified: score > 5.0`) are both production-dead and exist only for the crate's tests — under AGENTS.md's 'tests must exercise a real production path' bar, deleting them is aligned, not a loss. Note that CascadeConfig itself is not fully dead: production overrides bm25_min_candidates (1 vs default 2), and bm25_top_k/nli_threshold are read in cascade.rs:165,235 — the claim correctly targets only max_concurrent_nli. One side-effect nuance the claimant slightly understated: the three cascade tests in adapters.rs:316-461 pin BM25-only semantics (verified iff bm25>5.0) that the simplified always-lexical path replaces with entailment>=0.5 && contradiction<0.5, so the rewrites change assertion thresholds rather than just constructor calls, and the pub Bm25Verifier/VerificationInput exports in lib.rs:18 also disappear (no external users). Still small, self-contained breakage; the proposed collapse is strictly simpler and behavior-preserving for the single production configuration.

---

### 17. SigningKeyVault trait and LocalSigningKeyVault have zero consumers

**Area:** lineage / ocsf / observability
effort: **small** · finder confidence: **high** · ~LOC removable: **~85**

**Locations**

- `crates/moa-lineage/audit/src/signing.rs:135-218`
- `crates/moa-lineage/audit/src/lib.rs:30`

**What it is.** An `#[async_trait] SigningKeyVault` abstraction (get/rotate/list by label) with exactly one implementation, `LocalSigningKeyVault`, which manages 32-byte seed files on disk with create-on-miss and label listing. Neither the trait nor the impl is referenced anywhere in the workspace — not by production code, not by any test — beyond the lib.rs re-export.

**Why it may be over-engineered.** This is a speculative KMS-shaped abstraction with no concrete second implementation planned in-tree and no first caller. Actual audit-root signing gets its key from `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX` env material fed into `SigningKey::from_seed` (moa-edge/src/routes/lineage.rs:305, moa-core config overlay), bypassing the vault entirely.

**Simpler alternative.** Delete the trait, LocalSigningKeyVault, load_or_create_seed, and deterministic_seed. Keep `SigningKey`/`AuditRootSignaturePayload`, which are the parts production uses. Introduce a vault abstraction only when a real KMS integration lands.

**Side effects / what to watch.** None observable — no callers exist. The async-trait dependency may become removable from the crate if nothing else uses it.

**Value of simplifying.** ~85 lines and one speculative abstraction layer gone from a compliance-sensitive crate, shrinking the surface the promised external crypto review must cover.

**Adversarial verifier: ✅ CONFIRMED.** 1) Factual accuracy: verified. /Users/hwuiwon/Github/moa/crates/moa-lineage/audit/src/signing.rs:135-218 contains exactly what the claim describes: `#[async_trait] pub trait SigningKeyVault` (get/rotate/list, lines 136-144), `LocalSigningKeyVault` managing `{label}.seed` 32-byte files (147-196), plus private helpers `load_or_create_seed` (198-214) and `deterministic_seed` (216-218). 2) Hidden consumers: none. A repo-wide grep over all file types (code, Cargo.toml, docs, compose/yaml, scripts, .env.example; excluding target/.git/graphify-out) for `SigningKeyVault|LocalSigningKeyVault` returns only the definition lines in signing.rs and the single re-export at crates/moa-lineage/audit/src/lib.rs:30. A graphify BFS from those nodes shows zero incoming edges from outside signing.rs. Neither the crate's own tests (tests/lineage_audit_offline/, tests/merkle_publisher_db.rs) nor inline tests touch the vault — they all call `SigningKey::from_seed` directly. 3) Production path confirmed: /Users/hwuiwon/Github/moa/crates/moa-edge/src/routes/lineage.rs:303-339 builds the audit-root key via `configured_signing_key_from_config("MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX", ...)` -> `SigningKey::from_seed(label, seed)`, with the env var plumbed through moa-core config overlay (crates/moa-core/src/config/env_overlay.rs:168,784; config/mod.rs:236,452) and documented in docs/01-architecture-overview.md:459. The vault is bypassed entirely. 4) No load-bearing constraint forces it: no Restate/replay involvement, no doc or security mandate references the vault; if anything `deterministic_seed` (blake3 of a fixed prefix + label) yields predictable dev-only keys, so it must never carry the production path anyway. AGENTS.md rule 7 explicitly forbids speculative abstractions kept for hypothetical future paths, and the repo is pre-prod with no backwards-compat requirement. 5) Side effects: the claimant's hedge is resolvable — the only two `async_trait` uses in the crate are both in signing.rs, so `async-trait` (crates/moa-lineage/audit/Cargo.toml:13) becomes definitively removable after deletion; `tokio::fs`/`Path`/`PathBuf` imports in signing.rs also go. The inline tests in signing.rs (lines 220-263) exercise only `SigningKey`/`AuditRootSignaturePayload`, which the proposal keeps, so nothing breaks. The simplification is safe and strictly simpler.

---

### 18. ct-merkle dependency exists only to keep an unused 'reference shape' function visible

**Area:** lineage / ocsf / observability
effort: **small** · finder confidence: **high** · ~LOC removable: **~35 + one dependency**

**Locations**

- `crates/moa-lineage/audit/src/merkle.rs:335-347,449-455`
- `crates/moa-lineage/audit/Cargo.toml:11`

**What it is.** `ct_sha256_root` builds an RFC-6962 SHA-256 Merkle root via the `ct-merkle` crate. Its module doc states it is linked 'so the RFC 6962 proof shape stays visible in the crate'; the compliance root MOA actually commits is the hand-rolled domain-separated BLAKE3 tree. The only caller of `ct_sha256_root` in the entire repo is its own smoke test, which asserts the root is 32 bytes long.

**Why it may be over-engineered.** A parallel Merkle implementation kept purely as in-code documentation is a near-duplicate 'just in case' path — and this one drags in a dependency the crate's own README and docs/01 explicitly flag as not audited by its authors, inside the crate whose attestation gate is blocked on external cryptographic review.

**Simpler alternative.** Delete `ct_sha256_root`, its re-export in lib.rs:26-29, the smoke test, and the `ct-merkle` (and possibly `sha2`, if the PII vault HMAC is its only other user — it is not, so keep sha2) dependency. If the RFC 6962 shape matters for future interop, record it as a doc comment or an issue instead of compiled code.

**Side effects / what to watch.** None at runtime; one fewer entry in Cargo.lock/workspace-hack and one fewer unaudited crypto dependency to explain to auditors.

**Value of simplifying.** Removes an unused crypto dependency from the audit crate and deletes a decoy code path that a future reader could mistake for the real committed root.

**Adversarial verifier: ✅ CONFIRMED.** Every factual element of the claim checks out against the real code. (1) crates/moa-lineage/audit/src/merkle.rs:335-347: `ct_sha256_root` builds a MemoryBackedTree::<Sha256, Vec<u8>> from ct-merkle and returns the root bytes; the module doc (lines 3-6) literally says ct-merkle "is also linked and exercised through ct_sha256_root so the RFC 6962 proof shape stays visible in the crate, but the compliance root committed by MOA is BLAKE3-256." (2) The production path `MerkleRootPublisher::publish_one_window` (line 194) calls `blake3_merkle_root`, never `ct_sha256_root`; the inclusion-proof/verify functions (blake3_inclusion_proof, verify_blake3_inclusion) are also BLAKE3-only. (3) A whole-workspace grep for ct_sha256_root/ct_merkle/MemoryBackedTree, plus a graphify BFS from `ct_sha256_root()`, found zero consumers outside merkle.rs: the only caller is the smoke test `ct_merkle_root_is_available_for_rfc6962_shape` (lines 449-455), which asserts only `root.len() == 32` — a tautological availability check that pins no production behavior (fails the AGENTS.md test value bar: "every test must exercise a real production path, assert exact observable behavior"). The lib.rs:28 re-export has no downstream users; no other crate depends on ct-merkle (only Cargo.lock entries; it is not in workspace-hack/Cargo.toml). (4) No load-bearing constraint forces it: no Restate determinism, test lane, or security doc requires the SHA-256 shape — on the contrary, lib.rs:8-15, crates/moa-lineage/audit/README.md:13, and docs/01-architecture-overview.md:467 all flag ct-merkle as explicitly unaudited inside the crate whose attestation gate is blocked on external cryptographic review, so deleting it shrinks the review surface. (5) The claimant's sha2 analysis is correct: sha2 must stay because vault.rs:14-19 uses Hmac<Sha256> for the PII vault. Two minor side effects the claimant missed, neither blocking: the deletion should also update the now-stale prose references to ct-merkle in crates/moa-lineage/README.md:16, crates/moa-lineage/audit/README.md:13, the lib.rs:13-15 attestation-gate doc, and docs/01-architecture-overview.md:467 (these are warnings about the dependency, not consumers), and the merkle.rs module doc lines 3-6 plus the `use ct_merkle::...`/`use sha2::Sha256` imports at lines 14 and 17 in merkle.rs. Cargo.lock loses the ct-merkle entry; workspace-hack is unaffected. The simplification is safe and strictly simpler.

---

### 19. Dead session-only skill distillation/improvement lane kept parallel to the experience-native lane

**Area:** skills / artifacts
effort: **medium** · finder confidence: **high** · ~LOC removable: **~1000 (incl. tests)**

**Locations**

- `crates/moa-skills/src/distiller.rs:113-217, 316-358 (maybe_distill_skill, maybe_distill_skill_with_learning, distill_skill_with_learning, session_failed, extract_task_summary, build_distillation_request)`
- `crates/moa-skills/src/improver.rs:49-113 (maybe_improve_skill, maybe_improve_skill_with_learning, improve_skill_with_learning)`
- `crates/moa-skills/src/proposals.rs:53-91 (SkillProposalOutcome — zero consumers anywhere; SkillProposalSource::session_only)`
- `crates/moa-skills/tests/distillation_db_memory.rs, crates/moa-skills/tests/improver_db_memory.rs, crates/moa-skills/tests/draft_proposals_db_memory.rs`

**What it is.** The skill-learning feature contains two full generation lanes: a session-based lane (count tool calls on raw session events, check SessionMeta status, extract task summary from the last user message, propose with SkillProposalSource::session_only()) and an experience-based lane (distill_skill_from_experience_with_learning / improve_skill_with_learning_for_sources keyed on assessed ExperienceRecord + attributions). Each lane has its own entrypoints, prompts builders, and learnability gates; maybe_distill_skill/maybe_improve_skill additionally construct their own PostgresSessionStore via create_session_store(config).

**Why it may be over-engineered.** Production code calls exactly one lane: moa-orchestrator/src/workflows/skill_learning.rs:169 calls distill_skill_from_experience_with_learning, which routes improvements through improve_skill_with_learning_for_sources. The six session-lane public entrypoints, session_only(), SkillProposalOutcome, and the session-keyed branch of deterministic_skill_candidate_id are reachable only from moa-skills' own tests (~750 test lines pinning a lane no product path executes). docs/09 describes the experience-native path as the current flow; the session lane is the superseded predecessor kept 'just in case'.

**Simpler alternative.** Delete maybe_distill_skill, maybe_distill_skill_with_learning, distill_skill_with_learning, session_failed, extract_task_summary, build_distillation_request/_user_prompt from distiller.rs; delete maybe_improve_skill, maybe_improve_skill_with_learning, improve_skill_with_learning from improver.rs and promote improve_skill_with_learning_for_sources to the crate API; delete SkillProposalSource::session_only and the unused SkillProposalOutcome enum; simplify deterministic_skill_candidate_id to always key on experience IDs. Port the few genuinely valuable behavior pins (advisory-lock dedupe, UNCHANGED handling, name-change rejection) onto the experience lane in the existing test harnesses.

**Side effects / what to watch.** distillation_db_memory/improver_db_memory/draft_proposals_db_memory tests must be rewritten to drive the experience entrypoint (mostly mechanical: build an ExperienceRecord fixture instead of SessionMeta+events, both already exist in distiller unit tests). Loses the ability to distill directly from an unassessed session — which nothing uses and the docs say should not happen.

**Value of simplifying.** Deletes an entire duplicate learning pipeline (~350 src LOC + ~750 test LOC), removes one hidden store-construction path (create_session_store inside the library), and leaves a single answer to 'how does a skill proposal get created'.

**Adversarial verifier: ✅ CONFIRMED.** Verified every cited location plus workspace-wide consumer search (graphify call graph + grep across crates/, docs/, scripts/, compose files). (1) Factual accuracy holds: crates/moa-skills/src/distiller.rs:113-217 contains the session lane exactly as described (count_tool_calls on raw events, session_failed checking SessionMeta::status, extract_task_summary from last UserMessage/QueuedMessage, SkillProposalSource::session_only() at line 212), and maybe_distill_skill:119 / maybe_improve_skill (improver.rs:56) each build their own PostgresSessionStore via create_session_store(config). (2) No hidden consumers: a workspace grep for maybe_distill_skill|maybe_improve_skill|distill_skill_with_learning|improve_skill_with_learning|SkillProposalOutcome|session_only outside moa-skills returned zero hits (positive control: distill_skill_from_experience_with_learning correctly matched crates/moa-orchestrator/src/workflows/skill_learning.rs:169). The only production caller is skill_learning.rs:169 -> experience lane -> improve_skill_with_learning_for_sources. It is worse than claimed: the four maybe_* wrappers have ZERO callers anywhere including tests (tests/distillation_db_memory.rs, improver_db_memory.rs, draft_proposals_db_memory.rs call only distill_skill_with_learning and improve_skill_with_learning). SkillProposalOutcome (proposals.rs:55) matched only its own definition line in the entire workspace — fully dead; note the similarly named SkillProposalGeneration in distiller.rs:65 IS used by the orchestrator (lines 14-15, 296-319) and must be kept. (3) No load-bearing constraint forces the session lane: Restate determinism is preserved because the experience lane always passes vec![input.experience.id] (distiller.rs:248), so deterministic_skill_candidate_id (candidates.rs:12) keeps a non-empty experience-ID key and the empty/session branch (lines 34-36) becomes unreachable; the advisory-lock dedupe (proposals.rs:122,184) lives in store_skill_draft_proposal, shared by both lanes and retained. docs/09-skills-and-learning.md:186-236 confirms distillation runs "after qualifying experience persistence" with ExperienceRecord as the learning unit. Feature gating (lib.rs cfg skill-learning) is unaffected. Repo is pre-prod with an explicit no-backwards-compat rule. (4) The alternative works and is genuinely simpler — no complexity moves, roughly 200 lines of src plus ~750 lines of tests pinning a dead lane are removed. One visibility nuance: SkillProposalSource, session_only(), and improve_skill_with_learning_for_sources are already pub(crate), not public API; "promote to crate API" means making improve_skill_with_learning_for_sources pub so the improver_db_memory integration tests (external test binaries) can drive it, or alternatively driving improvement through distill_skill_from_experience_with_learning with a seeded similar skill as distillation_db_memory.rs:98 already does. (5) Minor side effects the claimant missed, none blocking: DistillationSkipReason::Failure and session_failed become dead and should be deleted along with the inline unit tests session_distillation_failure_uses_final_status_not_historical_tool_errors and skill_distillation_request_keeps_session_evidence_out_of_system_prompt (re-pin the prompt split on build_experience_distillation_request); candidates.rs unit tests at lines 112-150 pin the session-keyed branch and need updating; tests/support/common.rs fixtures (load_session_fixture, failed_session) become partially unused; docs/09 "Current generation flow" step 2 ("Extract a task summary from recent user input") describes the session lane and needs a one-line doc update. The orchestrator needs no change — it formats skip reasons via Debug ({reason:?}, skill_learning.rs:316) and imports only experience-lane symbols.

---

### 20. LearningReviewStore hand-rolls boxed-future trait plumbing and carries a never-called method; moka/async-trait deps unused

**Area:** skills / artifacts
effort: **small** · finder confidence: **high** · ~LOC removable: **~80**

**Locations**

- `crates/moa-skills/src/review.rs:22-60 (LearningReviewStoreFuture, LearningReviewStore, append_learning)`
- `crates/moa-skills/Cargo.toml:13,24 (async-trait, moka in [dependencies])`

**What it is.** LearningReviewStore is defined with a hand-rolled `Pin<Box<dyn Future + Send + 'a>>` type alias and explicit-lifetime methods, forcing every implementation (production SessionLearningReviewStore in the orchestrator plus two test doubles) to write Box::pin(async move { ... }) boilerplate for five methods. One of the five, append_learning (the non-transactional variant), is never called by any review helper — only append_learning_in_tx is used, so all three impls stub it (test doubles literally implement it as unreachable!()). Meanwhile moa-skills' Cargo.toml declares async-trait and moka as regular dependencies that no source file uses.

**Why it may be over-engineered.** The boxed-future encoding is a manual re-implementation of exactly what the already-declared async-trait crate (or native async fn in trait, since callers take `&impl LearningReviewStore + ?Sized` generically) provides. The dead append_learning method exists only for symmetry with its _in_tx twin. moka (a caching library) is dead weight in the dependency graph of a crate that has no cache.

**Simpler alternative.** Rewrite the trait with #[async_trait] (already a dependency) or plain async fns, delete the LearningReviewStoreFuture alias, and drop the append_learning method — keeping only get_learning_candidate, update_learning_candidate_status_from, update_learning_candidate_status_from_in_tx, and append_learning_in_tx. Remove moka (and async-trait if the native-AFIT route is taken) from [dependencies].

**Side effects / what to watch.** Signature churn in moa-orchestrator's SessionLearningReviewStore and the two test doubles (mechanical). If a dyn-object use of the trait exists somewhere with native AFIT, use #[async_trait] instead — behavior identical.

**Value of simplifying.** Cuts ~60 lines of Pin/Box ceremony across four sites, deletes a method whose only implementations are unreachable!(), and trims two unused crate dependencies from the build.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy — verified against the real code. crates/moa-skills/src/review.rs:22-60 defines LearningReviewStoreFuture as Pin<Box<dyn Future<Output=Result<T, MoaError>> + Send + 'a>> and a 5-method trait with explicit-lifetime signatures, exactly as claimed. All three implementations write Box::pin(async move {...}) for every method: production SessionLearningReviewStore (crates/moa-orchestrator/src/services/learning_review.rs:154-206), RecordingReviewStore (review.rs:622-674), FailingLearningAppendStore (crates/moa-orchestrator/tests/skill_learning_review_db.rs:434-489). Cargo.toml lines 13 and 24 declare async-trait and moka in [dependencies]; a recursive grep of moa-skills src/ and tests/ finds zero moka usage anywhere and async_trait used only in tests/support/common.rs, which is already covered by the separate [dev-dependencies] async-trait entry (Cargo.toml:38).

Dead method — confirmed. review.rs helpers call only append_learning_in_tx (line 280); no code anywhere dispatches LearningReviewStore::append_learning through the trait. The other append_learning hits in the workspace (moa-orchestrator/src/workflows/consolidate.rs:582, turn_execution.rs:2391, moa-session/src/store/learning.rs) are the separate moa-core SessionLearningLogStore trait / PostgresSessionStore inherent method, untouched by removing the trait method. One factual nit in the claim: only RecordingReviewStore stubs append_learning as unreachable!(); SessionLearningReviewStore and FailingLearningAppendStore delegate it to PostgresSessionStore::append_learning — but since nothing calls it via the trait, those are still dead code and the conclusion stands.

Hidden consumers — none. grep for LearningReviewStore/LearningReviewStoreFuture across the workspace (confirmed via graphify query first) finds only the three files above. grep "dyn LearningReviewStore" finds nothing: every call site is generic `&(impl LearningReviewStore + ?Sized)`, so no dyn-compatibility constraint exists.

Load-bearing constraints — none. The store calls execute inside Restate ctx.run durable-step closures (learning_review.rs:68-139); boxed-vs-macro-vs-AFIT async encoding has zero effect on replay determinism. moa-core's own store traits (e.g. SessionLearningLogStore at crates/moa-core/src/traits/mod.rs:306, and #[async_trait] at lines 42/210/226/248) already use #[async_trait] including &mut-connection-style borrows, so review.rs is the repo's outlier, not its pattern. The alias doc comment's rationale ("keep transaction-borrow lifetimes explicit") is exactly what #[async_trait]'s desugaring produces automatically for the &'a mut PgConnection in_tx methods.

Proposed alternative works, with one route-choice caveat the claimant slightly misattributed: they said use #[async_trait] "if a dyn-object use exists"; the actual reason to prefer #[async_trait] (or AFIT with explicit `-> impl Future<...> + Send` return bounds) is that the review helpers' futures must be Send because they run inside Restate ctx.run closures — plain `async fn` in trait carries no Send bound and would fail to compile at those call sites (a loud compile error, not silent breakage). #[async_trait] is the mechanical drop-in and is already a workspace dependency. Side effects the claimant missed are trivial: keep async-trait in [dev-dependencies] (tests/support/common.rs needs it), and regenerate workspace-hack (cargo hakari) after removing moka. Pre-prod repo with an explicit no-backwards-compat policy, so the signature churn in the three impls is acceptable by design.

---

### 21. Regression module ships report/decision types and a file-writing suite generator that production never uses

**Area:** skills / artifacts
effort: **small** · finder confidence: **high** · ~LOC removable: **~120**

**Locations**

- `crates/moa-skills/src/regression.rs:36-75 (SkillRegressionDecision, SkillRegressionReport, accepted())`
- `crates/moa-skills/src/regression.rs:92-106 (generate_skill_test_suite, tokio::fs writer)`
- `crates/moa-skills/tests/regression.rs:59-68`

**What it is.** Alongside the used pieces (generate_skill_test_suite_source, SkillRegressionSummary, compare_scores), regression.rs defines a 5-variant SkillRegressionDecision enum, a SkillRegressionReport struct with an accepted() policy helper, and an async generate_skill_test_suite that materializes the suite TOML onto the local filesystem under config.local.memory_dir.

**Why it may be over-engineered.** The orchestrator's actual regression executor (moa-orchestrator/src/services/skill_regression.rs) consumes only SkillRegressionSummary and compare_scores; it builds its own JSON report and never constructs SkillRegressionReport or SkillRegressionDecision — their only consumer is moa-skills' own tests/regression.rs. The file-writing generate_skill_test_suite has zero callers anywhere and directly contradicts docs/09: 'moa-skills only generates reviewable regression suite source... without writing or running the suite.' It is also the only reason regression.rs touches tokio::fs and MoaConfig.

**Simpler alternative.** Delete SkillRegressionDecision, SkillRegressionReport, accepted(), and generate_skill_test_suite (plus the report-shaped assertions in tests/regression.rs). Keep GeneratedSkillSuite, generate_skill_test_suite_source, SkillRegressionSummary, and compare_scores — the surface production actually uses.

**Side effects / what to watch.** None at runtime. If a future decision-enum is wanted for the eval runner it belongs next to the runner in moa-orchestrator, which already encodes its decisions in JSON evaluation payloads.

**Value of simplifying.** Removes an undocumented filesystem side-effect path from a library crate and ~100 lines of speculative reporting API, aligning the module with its documented single job: generate reviewable TOML source.

**Adversarial verifier: ✅ CONFIRMED.** Every factual assertion in the claim checks out against the real code. (1) crates/moa-skills/src/regression.rs:36-75 defines the 5-variant SkillRegressionDecision enum and SkillRegressionReport with accepted(); lines 92-106 define async generate_skill_test_suite, which calls tokio::fs::create_dir_all/write under config.local.memory_dir. (2) Hidden consumers: a workspace-wide grep (*.rs, *.toml, *.md, *.sh, *.yml, scripts/) plus a graphify BFS query both show SkillRegressionDecision and SkillRegressionReport appear ONLY in crates/moa-skills/src/regression.rs and crates/moa-skills/tests/regression.rs:9-10,59-68; generate_skill_test_suite (the file-writing variant) has zero callers anywhere — not even a test. The orchestrator's executor imports exactly `regression::{SkillRegressionSummary, compare_scores}` (crates/moa-orchestrator/src/services/skill_regression.rs:33), and learning_review.rs consumes the orchestrator's own skill_acceptance_regression_report, not the moa-skills report type. (3) The surface the claim keeps is genuinely used: generate_skill_test_suite_source is called from crates/moa-skills/src/distiller.rs:205,290 and crates/moa-skills/src/improver.rs:169, and compare_scores/SkillRegressionSummary from the orchestrator. (4) No load-bearing constraint protects the dead code: docs/09-skills-and-learning.md:189-191 states "moa-skills only generates reviewable regression suite source" and lines 205-206 say the suite TOML is stored in the candidate payload "without writing or running the suite" — the async file-writer directly contradicts the documented architecture rather than implementing it. No Restate-determinism, performance, or security doc depends on these types. AGENTS.md's no-compat-shim/pre-prod stance supports a clean deletion. (5) Bonus simplification the claimant correctly anticipated: MoaConfig, SessionMeta (from the moa_core import at regression.rs:5) and the `use tokio::fs` at line 7 are used only by generate_skill_test_suite, so deletion also removes those imports from the module (PathBuf stays, used by skill_suite_relative_path and GeneratedSkillSuite path building — trivially adjustable). Deleting the report-shaped test at tests/regression.rs:57-68 is already in the proposal; the other two tests in that file exercise the kept surface and survive unchanged. No side effects were missed; the claim is confirmed as stated.

---

### 22. control.rs and edit_window.rs are dead modules with test-only consumers

**Area:** edge / messaging / security
effort: **small** · finder confidence: **high** · ~LOC removable: **~430**

**Locations**

- `crates/moa-messaging/src/control.rs`
- `crates/moa-messaging/src/edit_window.rs`
- `crates/moa-messaging/tests/messaging_offline/control_signals.rs`
- `crates/moa-messaging/tests/messaging_offline/edit_window.rs`
- `crates/moa-messaging/tests/support/control_signals.rs`
- `crates/moa-messaging/tests/support/edit_window.rs`

**What it is.** control.rs translates inbound /stop, /stop --force, and /queue slash commands into SessionSignal control actions with per-channel acknowledgements. edit_window.rs provides edit_with_followup_fallback, a generic (EditFn/FollowFn type-parameterized) edit-then-thread-reply fallback with MessagingEditOutcome/MessagingEditResponse wrapper types.

**Why it may be over-engineered.** Workspace-wide grep shows the only callers of control_action_for_inbound, edit_with_followup_fallback, MessagingEditOutcome, and MessagingEditResponse are the crate's own offline tests. No orchestrator or contacts code routes slash commands through control.rs, and SlackAdapter::edit implements its own chunked edit/delete path (edit_locked) that never consults edit_with_followup_fallback. Both are speculative features kept compiling by pub exports so no dead_code lint fires; edit_window additionally uses two generic closure type parameters for a function with zero call sites.

**Simpler alternative.** Delete both source modules, their lib.rs re-exports, and the four test/support files. Reintroduce command routing when the Slack inbound path actually dispatches SessionSignals (docs/implementation-caveats.md already flags that interactive actions should stop being normalized into control messages, i.e. this design is slated to change anyway).

**Side effects / what to watch.** None at runtime — nothing executes this code. Loses ~10 tests that pin behavior of unwired code. If /stop support lands later it will likely be rebuilt against typed callback events per the caveats doc, not this text-command parser.

**Value of simplifying.** Pure deletion of two unwired features (~200 src + ~230 test lines); removes the misleading impression that slash commands and edit-window fallback are live product behavior.

**Adversarial verifier: ✅ CONFIRMED.** Factual accuracy verified by reading both modules and tracing all consumers (graphify BFS depth-2 from both functions plus workspace-wide grep). (1) The only references to control_action_for_inbound, MessagingControlAction, edit_with_followup_fallback, MessagingEditOutcome, MessagingEditResponse, and is_fallback_edit_error outside the two source files are the re-exports in crates/moa-messaging/src/lib.rs (lines 7, 9, 23, 28-30), the crate's own offline tests (crates/moa-messaging/tests/messaging_offline/control_signals.rs, .../edit_window.rs, and their support files), and the generated graphify-out/COMMUNITY_LABELS.md. No orchestrator, edge, or contacts code routes inbound text through control.rs; the only InboundMessage use outside moa-messaging is a test mock in crates/moa-orchestrator/src/workflows/progress_delivery.rs (test module, line 328). (2) SlackAdapter's real edit path is independent: edit() at crates/moa-messaging/src/slack.rs:583 delegates to edit_locked (L600-651), a chunked update/append/delete implementation with zero references to edit_window symbols; the only message_not_found handling in slack.rs is delete_locked (L667) using the typed SlackClientError::ApiError code, not edit_window's body-substring matching — confirming the fallback logic in edit_window.rs is a parallel, unused design. (3) No load-bearing constraint: SessionSignal::SoftCancel/HardCancel/QueueMessage live in moa-core (types/session.rs:101-105) and are consumed by moa-brain streaming signals independently of control.rs, so deletion cannot affect Restate determinism or the brain loop. No architecture doc mandates /stop, /queue, or an edit-window followup fallback (grep of docs/*.md found nothing), and docs/implementation-caveats.md lines 11-17 explicitly flag text-control-message normalization as a design to be replaced with typed structured callback events — i.e., the deleted parser would be rebuilt differently anyway, exactly as the claimant argued. (4) The simpler alternative works cleanly: delete src/control.rs, src/edit_window.rs, the lib.rs mod/pub-use lines, the two mod declarations in tests/messaging_offline.rs (lines 4-11), the two test files, and the two support files (each support file is consumed only via its own test module's #[path = "../support/..."] mod support). Minor corrections that do not change the verdict: the lost test count is 6 (4 control_signals + 2 edit_window), not ~10; the harness file tests/messaging_offline.rs also needs its two mod declarations removed; and graphify-out should be refreshed after the deletion.

---

### 23. Analytics wire response structs duplicate domain DTOs field-for-field, with hand-written copy mappers at the edge

**Area:** edge / messaging / security
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~200**

**Locations**

- `crates/moa-edge/src/routes/analytics.rs (L506-588)`
- `crates/moa-core/src/wire/analytics.rs (L17-170)`
- `crates/moa-core/src/analytics.rs (L9-140)`

**What it is.** moa-core defines both domain analytics DTOs (SessionAnalyticsSummary, TenantAnalyticsSummary, ToolCallSummary, CacheDailyMetric — no serde) and wire twins (SessionStatsResponse, TenantStatsResponse, ToolStatsRow, CacheDailyMetricRow — serde-derived) with identical fields. moa-edge handlers then copy every field across via session_stats_response_from_summary, tenant_stats_response_from_summary, tool_stats_response_from_rows, and cache_stats_response_from_parts.

**Why it may be over-engineered.** The only purpose of a wire/domain twin layer is to let the wire format evolve independently of internal types — a backwards-compatibility concern MOA explicitly does not have pre-production. The wire response structs are consumed solely by moa-edge and its one integration test; the domain structs are already documented as 'read-model DTOs shared by session storage and API surfaces'. Every added analytics field must currently be written three times (domain struct, wire struct, mapper).

**Simpler alternative.** Derive Serialize/Deserialize on the domain DTOs and return them directly from the edge handlers (wrap in the existing envelope where a response adds fields, e.g. keep a thin CacheStatsResponse { days, estimated_savings_cents, summary fields, daily: Vec<CacheDailyMetric> } or drop estimated_savings_cents, which is hardcoded to None). Delete the four twin structs and four mapping functions; keep the request types, which carry real wire-only shape (tenant injection, optional filters).

**Side effects / what to watch.** JSON field names stay identical (same field identifiers), so the edge test suite should pass with minimal edits; moa-core's wire module shrinks; any future deliberate wire-format divergence would reintroduce a mapping for just that field.

**Value of simplifying.** Removes ~200 lines of triple bookkeeping and the class of bugs where a new analytics column is added to one twin but not the other.

**Adversarial verifier: ✅ CONFIRMED.** Every cited location checks out. crates/moa-core/src/wire/analytics.rs (L17-49, L62-81, L105-118, L155-174) and crates/moa-core/src/analytics.rs (L9-40, L44-57, L98-117, L121-140) are field-for-field identical twins (same names, types, doc comments), and crates/moa-edge/src/routes/analytics.rs L506-588 contains the four pure copy mappers, with estimated_savings_cents hardcoded to None at L572. Consumer search: the four wire response structs are used ONLY by moa-edge/src/routes/analytics.rs (Json responses at L76, L117, L185, L237) and one integration test, crates/moa-edge/tests/direct_read_routes_db.rs (deserializes ToolStatsResponse) — no orchestrator, messaging, loadtest, script, compose, or doc consumer. These handlers are direct DB reads at the edge (the test file is literally named direct_read_routes_db), not forwarded to a Restate service, so no cross-service wire hop exists that would need an independent wire format; Restate replay determinism (docs/02) is untouched. No enforcement mechanism mandates the twin layer: xtask/src/check_architecture_boundaries.rs has no wire-vs-domain or serde rule (its L2188 analytics mention is a synthetic string inside a re-export-budget unit test), and docs/03-communication-layer.md says nothing about wire/domain twinning. Strongest refutation of any 'deliberate boundary' theory: the codebase already uses the proposed pattern — LearningCandidateSummary, a serde wire type from wire/analytics.rs, is returned directly by the SessionStore trait (crates/moa-core/src/traits/mod.rs L35) and constructed directly in crates/moa-session/src/analytics.rs; SessionTurnMetric (domain, no twin) shows the twin layer is not consistently applied either. Pre-prod no-backwards-compat policy removes the only rationale for the layer. The proposal works and is genuinely simpler; JSON field names are identical so the wire format is unchanged. Minor side effects the claimant understated: (1) carry over #[serde(default)] on Option/Vec fields (contact_id, daily, rows) when deriving serde on the domain DTOs; (2) the edge test's `use moa_core::wire::analytics::ToolStatsResponse` import must follow wherever the kept envelope moves; (3) moa-lineage/core/src/records.rs L396 defines an unrelated struct also named ToolCallSummary — different crate, already coexists, no conflict, but avoid glob imports. None of these change the verdict.

---

### 48. runtime/endpoint.rs builds a function-pointer service registry to express a static bind list

**Area:** Orchestrator — services/rest
effort: **small** · finder confidence: **high** · ~LOC removable: **~380**

**Locations**

- `crates/moa-orchestrator/src/runtime/endpoint.rs:60-275`
- `crates/moa-orchestrator/src/runtime/endpoint.rs:277-477`

**What it is.** Binding the ~35 Restate services is done through a data-driven registry: a RestateBinding descriptor struct holding a name and an Option<fn(EndpointBuilder, &EndpointBindingContext) -> EndpointBuilder>, five const descriptor arrays (head/body/tail plus per-feature variants with 'name_only' descriptors whose bind is None), a restate_bindings_for_features(bool,bool,bool) assembler, a fold over the builder with .expect("compiled feature bindings must include a bind function"), and ~35 one-line bind_* wrapper functions. ~200 further lines of tests exercise the registry mechanics across simulated feature-bool combinations.

**Why it may be over-engineered.** In production code the three bools are always cfg!(feature = ...) compile-time constants; the bool parameterization and the name_only descriptors exist only so #[cfg(test)] helpers can compute service-name lists for feature combinations that are not even compiled in. The Option<fn> + runtime .expect() introduces a panic path for a property (every compiled binding has a bind fn) that a plain builder chain makes unrepresentable. Each bind_* wrapper is one line; the registry adds a moving part (fn-pointer table) where the Restate SDK's builder chain already is the registry.

**Simpler alternative.** Write build_endpoint as a literal chain: Endpoint::builder().bind(SessionStoreImpl::new(...).serve()).bind(LLMGatewayImpl::new(...).serve())... with #[cfg(feature)] blocks for Eval/Experiments/SkillLearning, and expected_service_names() as a vec!["SessionStore", ...] with cfg-gated pushes. Keep one test asserting expected_service_names() has no duplicates and matches the feature cfgs; drop the feature-bool simulation tests (name lists for non-compiled features verify nothing real).

**Side effects / what to watch.** The lib test pinned by scripts/run-clean-e2e.sh line 326 (runtime::endpoint::tests::skill_learning_feature_adds_skill_learning_workflow) would be renamed/replaced and the script updated. Risk of the bind list and name list drifting apart is covered by the readiness check itself plus one remaining test.

**Value of simplifying.** Cuts the 696-line file roughly in half, removes a function-pointer indirection layer and a runtime panic path from binary startup, and makes adding a service a one-line diff instead of descriptor + wrapper fn + name entry.

**Adversarial verifier: ✅ CONFIRMED.** `RestateBinding { name, bind: Option<fn> }`, descriptor arrays, `cfg!` booleans, and the registry fold exist in `crates/moa-orchestrator/src/runtime/endpoint.rs:60`, `:93`, and `:204`. Simplifying to a literal builder chain plus cfg-gated expected service names is viable, while preserving the `services_registered()` readiness contract used by startup/jobs.

---

### 52. Two overlapping compaction subsystems both own LLM checkpoint emission (stage-8 HistoryCompiler and stage-10 Compactor)

**Area:** moa-brain (context pipeline)
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~400**

**Locations**

- `crates/moa-brain/src/pipeline/history/compaction.rs:16-93`
- `crates/moa-brain/src/pipeline/compactor/mod.rs:33-163`
- `crates/moa-brain/src/pipeline/compactor/summarize.rs:18-71`
- `crates/moa-brain/src/pipeline/compactor/deterministic.rs:9-135`
- `crates/moa-brain/src/pipeline/compactor/triggers.rs`
- `crates/moa-brain/src/pipeline/builder.rs:172-190`
- `crates/moa-brain/src/pipeline/builder.rs:260-264`
- `crates/moa-brain/src/compaction.rs:94-113`

**What it is.** The production pipeline wires the SAME CompactionConfig and the SAME compaction LLM provider into two separate stages. Stage 8 (HistoryCompiler) has a watermark gate that reads the full event log and emits an LLM Checkpoint event inline (history/compaction.rs). Stage 10 (Compactor) is a second, message-level compaction subsystem with three tiers; its tier 3 re-reads the ENTIRE event log a second time in the same turn (summarize.rs:25-27) and calls the same maybe_compact_events helper, force-overriding the config (`event_threshold = 1`, `token_ratio_threshold = 0.0`) to bypass the trigger logic that the history stage uses. The two stages also duplicate boundary logic (recent_turn_boundary over events in compaction.rs vs recent_turn_boundary_messages over messages in triggers.rs) and are coupled through duplicated magic placeholder strings: deterministic.rs:13 redeclares its own copy of FILE_READ_DEDUP_PLACEHOLDER (also defined in history/mod.rs:41-42) and sniffs message CONTENT for "[showing lines " and elision placeholders to stay idempotent with what stage 8 already pruned.

**Why it may be over-engineered.** Compaction has two owners with duplicated triggers, duplicated recent-turn-boundary implementations, a config-override hack to defeat the shared gate, a redundant second full-log read per triggering turn, and cross-stage coordination via copy-pasted magic strings (if either placeholder constant drifts, the compactor silently re-elides or double-counts). Stage 8 already prunes tool output, dedups file reads, applies checkpoints, and emits new checkpoints; stage 10 re-does elision and checkpointing on the compiled messages. Only tier 3's trigger (final token count vs per-turn ceiling) genuinely needs post-history information, and the history stage already knows its own compiled token stats within a small margin.

**Simpler alternative.** Make the history stage the single compaction owner: keep the watermark-gated checkpoint emission where it is, add the token-ceiling (tier3) condition to that same gate, and run the deterministic tool-result elision (tier1/tier2) as plain functions at the end of history compilation where the placeholder constants already live. Delete the stage-10 Compactor processor, summarize.rs's forced-config re-trigger and second full-log read, triggers.rs's parallel boundary logic, and the duplicated placeholder constants. The compaction report metadata can be emitted from the history stage's ProcessorOutput.

**Side effects / what to watch.** Docs/07 stage table changes (11 stages instead of 12); compactor unit tests and tier-3 tests move into the history harness; pipeline_stages_db_memory and cache-replay tests that assert compactor metadata keys need updating; the snapshot-collapse interplay (snapshot.rs) must move with tier2. Behavior risk: tier-3 emergency compaction would fire slightly earlier (before RuntimeContext's few tokens are appended) unless a margin is kept.

**Value of simplifying.** Removes a whole pipeline stage and a second full event-log read on compaction turns, collapses two trigger systems into one, and eliminates cross-module magic-string coupling that can silently break elision idempotency. Roughly 350-450 lines deleted net.

**Adversarial verifier: ✅ CONFIRMED.** The production builder wires both `HistoryCompiler::with_compaction` and a later `Compactor::new` into one pipeline (`moa-brain/src/pipeline/builder.rs:172`, `:260`), and both can emit checkpoints through the same helper. The simplification is valid, with docs/01, docs/05, prompt-caching docs, docs/17, and compaction stage metrics to update/preserve.

---

### 58. MemoryScope::ancestors() is an identity function feeding a vestigial scope_ancestors chain

**Area:** moa-memory (graph/ingest/lifecycle/pii/vector)
effort: **small** · finder confidence: **high** · ~LOC removable: **~40**

**Locations**

- `crates/moa-memory/types/src/lib.rs:34-49`
- `crates/moa-brain/src/planning/planner.rs:68-70,181,734`
- `crates/moa-brain/src/retrieval/cache.rs:416-424,891`

**What it is.** MemoryScope::ancestors() returns vec![self.clone()] in both match arms — a single-element Vec of the scope itself. It populates PlannedQuery::scope_ancestors ('Ancestor chain from global through the most-specific scope'), whose only consumer is a retrieval cache-key builder that loops over the Vec to append 'layers=' — always emitting exactly the same scope that the adjacent 'scope=' segment already contains.

**Why it may be over-engineered.** This is the residue of a removed scope-inheritance design: docs/04 now states contact sessions do NOT inherit tenant admin/operator memory or other scopes as implicit ancestors. The Vec, the method, and the field model a hierarchy that no longer exists, and the doc comment on the field is actively wrong.

**Simpler alternative.** Delete ancestors() and the scope_ancestors field; the cache key already includes the scope. If a call site needs a Vec, construct vec![scope] inline (only the eval runner and loadtest scenario builders do).

**Side effects / what to watch.** Retrieval cache keys change shape (harmless pre-prod); a handful of test fixtures in moa-brain/moa-eval/moa-loadtest drop one struct field; the types/tests/scope.rs ancestors test is deleted.

**Value of simplifying.** Small LOC but removes a misleading concept from the retrieval planner's core type — the field invites someone to 'fix' retrieval by populating a hierarchy the architecture explicitly rejected.

**Adversarial verifier: ✅ CONFIRMED.** `MemoryScope::ancestors()` returns only the scope itself (`moa-memory/types/src/lib.rs:34`), while `PlannedQuery::scope_ancestors` claims a global-to-specific chain and only feeds duplicate cache-key data. Docs reject implicit contact/tenant inheritance (`docs/04-memory-architecture.md:31`, `:154`). Delete the method/field and update constructors/tests.

---

### 59. PiiSpan.replacement Option exists only for backwards compatibility and never holds a non-default value

**Area:** moa-memory (graph/ingest/lifecycle/pii/vector)
effort: **small** · finder confidence: **high** · ~LOC removable: **~40**

**Locations**

- `crates/moa-memory/pii/src/lib.rs:102-148,175-205`
- `crates/moa-memory/ingest/src/slow_path.rs:479-490`

**What it was.** PiiSpan carried replacement: Option<String> with a serde default documented as 'older serialized spans may omit this field', a with_replacement constructor, and a redaction_replacement() accessor that fell back to the category constant. Every construction site in the workspace (PiiSpan::new, the openai_filter client, the heuristic classifier) set the canonical category replacement; the sole with_replacement caller in slow_path passed span.redaction_replacement() — i.e., the default again — inside a redaction_replacement() helper that wrapped the whole source text in a synthetic full-length span and called redact_text on it, which reduced to returning the replacement string unchanged.

**Why it was over-engineered.** The Option, the fallback accessor, and the serde default existed purely for wire compatibility with 'older serialized spans' — MOA is pre-production and has no such spans to honor. The replacement text is a pure function of category, so storing it per-span added a field that could only ever disagree with the category by bug. The slow_path helper was an identity wrapper.

**Implemented simplification.** Deleted the replacement field, with_replacement, and the accessor; redact_text now pushes redaction_replacement(span.category) directly. In slow_path, the redaction helper is gone and fact-part replacement uses redaction_replacement(span.category) directly.

**Side effects / what to watch.** Any journaled PiiResult values in in-flight Restate invocations change shape (transient, pre-prod); pii offline tests constructing spans drop one field.

**Value of simplifying.** Removes a compatibility knob with zero producers of non-default values and an identity-function indirection on the ingestion redaction path.

**Adversarial verifier: ✅ CONFIRMED.** `PiiSpan.replacement` existed for older serialized spans, but constructors always derived the category replacement and the only production `with_replacement` caller passed that same replacement back into an identity wrapper (`slow_path.rs:479`). The implemented cut deletes the field/constructor/accessor and uses `redaction_replacement(category)` directly, accounting for transient Restate journal shape changes.

**Implementation status: ✅ DONE.** Current code has no `with_replacement`, `span.redaction_replacement()`, or `PiiSpan { replacement: ... }` call sites. Focused verification passed with `cargo test -p moa-memory-pii --locked --lib`, `cargo test -p moa-memory-pii --locked --test memory_pii_offline`, and `cargo check -p moa-memory-ingest --locked`. The full `cargo test -p moa-memory-pii --locked` command is deferred because the DB-memory erasure lane timed out waiting for a maintenance database pool connection, tracked in `docs/simplification-deferred-regressions.md`.

---

### 61. Hand-rolled ~2,300-line env-to-config mapping layer (MoaEnvOverlay + 22 apply_* mirror functions)

**Area:** moa-core (traits/types/config/wire)
effort: **large** · finder confidence: **medium** · ~LOC removable: **~2000**

**Locations**

- `crates/moa-core/src/config/env_overlay.rs (1,333 lines)`
- `crates/moa-core/src/config/loader.rs`
- `crates/moa-core/src/config/providers.rs:120-250`
- `crates/moa-core/src/config/auth.rs:123-265`
- `crates/moa-core/src/config/context.rs:351-500`
- `crates/moa-core/src/config/memory.rs:257-338`
- `crates/moa-core/src/config/knowledge.rs:372-482`
- `crates/moa-core/src/config/session.rs:170-228 (plus ~14 more config modules with apply_* functions)`

**What it is.** MoaConfig::load() never loads TOML (loader.rs is Default::default() + env overlay only, despite AGENTS.md saying config is TOML via the `config` crate, which is not even a workspace dependency). Env config works by envy-deserializing a flat 245-field `MoaEnvOverlay` mirror struct (every field doc-commented with its MOA_* name), then hand-copying each field into the nested MoaConfig through 22 per-module `apply_*_overlay` functions using five bespoke setter helpers (set_if_some, set_copy_if_some, ...), plus a hand-maintained URL-validation table and an envy error-message prefix-restorer. Every new knob must be edited in 3+ places (nested config field, overlay mirror field, apply line, sometimes validate).

**Why it may be over-engineered.** The entire layer is a manual re-implementation of derived environment deserialization. Because env is the ONLY config source, the parallel Option-typed mirror struct plus per-field copy code adds no semantics beyond 'set nested field from env var'. The single-underscore flat naming (MOA_DATABASE_MAX_CONNECTIONS) is what forces the hand mapping: names are ambiguous against nested field paths, so nothing can be derived. It is a chronic drift/failure point (this audit found 119 of the 245 mirrored knobs are referenced nowhere else — see separate finding).

**Simpler alternative.** Switch env naming to an unambiguous separator (MOA__DATABASE__MAX_CONNECTIONS) and deserialize straight into the existing nested MoaConfig: either add the `config` crate's Environment source, or keep zero new deps with a ~40-line function that folds MOA__A__B=v pairs into a serde_json::Value tree and deserializes it over MoaConfig with #[serde(default)] on each section. Move the deliberate fan-out logic (e.g. MOA_COHERE_API_KEY propagating into memory.extraction and memory.vector.embedder.cohere) into a small MoaConfig::finalize() step, and URL checks into the existing validate(). Delete env_overlay.rs and all apply_* functions. MOA is pre-prod: renaming the ~77 env vars actually set in docker-compose*/k8s/Makefile/scripts is a permitted clean break.

**Side effects / what to watch.** Mechanical rename of MOA_* vars across docker-compose.yml, docker-compose.chaos.yml, k8s manifests, Makefile, scripts, .env.example, and docs; k8s secret names get slightly longer (double underscore). The env_overlay from_iter round-trip tests are replaced by a couple of fold-and-deserialize tests. Error messages for a bad value become serde-path style instead of the curated 'MOA_X value ... is invalid' text unless serde_path_to_error is used.

**Value of simplifying.** Deletes roughly 2,000 net lines of pure plumbing, removes the 3-places-per-knob drift trap, and makes AGENTS.md's config story true again; adding a config field becomes a one-line struct edit.

**Adversarial verifier: ✅ CONFIRMED.** `MoaConfig::load()` is env-only via `load_from_env()` and overlay application (`config/loader.rs:8`), while `MoaEnvOverlay` remains a flat 245-field `MOA_*` mirror with manual `apply_*_overlay` fan-out (`env_overlay.rs:14`, `:523`, `:545`, `:586`). No workspace `config` dependency exists. Proposal holds materially; current deploy-ish files mention 82 parsed overlay vars, not roughly 77.

---

### 63. Session persistence split into 7 single-impl 'focused contract' traits plus a blanket SessionRepository aggregate

**Area:** moa-core (traits/types/config/wire)
effort: **medium** · finder confidence: **high** · ~LOC removable: **~300**

**Locations**

- `crates/moa-core/src/traits/mod.rs:209-471`
- `crates/moa-orchestrator/src/ctx.rs:28-135`
- `crates/moa-eval/src/setup.rs:125-130`

**What it is.** Besides SessionStore, moa-core defines SessionChannelStore, SessionEventLookupStore, SessionAnalyticsStore, SessionLearningLogStore, SegmentStore, ExperienceStore, and LearningCandidateStore, aggregated by `trait SessionRepository: ...` with a blanket impl. Each of the 7 focused traits has exactly ONE implementation in the workspace: PostgresSessionStore (verified via grep; the only test doubles in the tree implement plain SessionStore). Orchestrator PersistenceDeps then stores the SAME Arc<PostgresSessionStore> cloned into 11 fields under different dyn types with 11 accessor methods, and moa-eval setup.rs performs the same upcasts.

**Why it may be over-engineered.** Interface segregation with no payoff: no second backend exists or is planned, no test fake implements any focused trait (so the narrowing buys no test seam), and all the traits live in moa-core so there is no dependency-graph decoupling either. The split's only concrete effect is ~260 lines of trait declarations, an aggregate trait + blanket impl, and an 11-way Arc/accessor mirror in ctx.rs that all resolve to one object.

**Simpler alternative.** Fold the 7 traits' methods into SessionStore, using the default-body `Err(MoaError::Unsupported)` pattern SessionStore already uses (store_text_artifact, update_session_contact) so the 5 existing MockSessionStore test doubles compile unchanged. Delete SessionRepository and its blanket impl. Shrink PersistenceDeps to `session_store: Arc<PostgresSessionStore>` (plus action_policy_store and graph_pool) with one accessor; consumers that currently take Arc<dyn SegmentStore> (moa-brain builder) or Arc<dyn LearningCandidateStore> (experiments) take Arc<dyn SessionStore>.

**Side effects / what to watch.** Wide but mechanical import/signature churn across moa-orchestrator, moa-brain, moa-eval, moa-edge. A merged trait means a hypothetical future non-Postgres analytics backend would need a trait re-split — trivial to do when it actually exists. Method-name collisions must be checked when merging (none observed).

**Value of simplifying.** Removes 8 trait definitions, a blanket impl, and an 11-field Arc mirror — one store concept instead of nine names for the same Postgres object; new store methods land in one place.

**Adversarial verifier: ✅ CONFIRMED.** The focused traits and aggregate exist (`moa-core/src/traits/mod.rs:209`, `:448`), each focused trait has exactly one concrete `PostgresSessionStore` implementation, and `PersistenceDeps` clones the same concrete store into multiple dyn slots. Docs already describe `SessionStore` as the conceptual owner of these responsibilities (`docs/01-architecture-overview.md:156`).

---

### 64. Four near-identical turn-message DTOs in wire/turn.rs, two of them field-for-field identical

**Area:** moa-core (traits/types/config/wire)
effort: **small** · finder confidence: **high** · ~LOC removable: **~60**

**Locations**

- `crates/moa-core/src/wire/turn.rs:31-62`
- `crates/moa-core/src/wire/turn.rs:167-253`

**What it is.** StartTurnRequest and QueueMessageRequest are field-for-field the same struct (user_message, attachments, model, contact, max_turns) under two names; PendingMessage repeats those 5 fields plus queued_at/identity; RunTurnRequest repeats them again plus turn ids/trigger. StartTurnResponse{turn_id, queued} and QueueMessageResponse{queued, started_turn_id} are also the same shape with swapped field names.

**Why it may be over-engineered.** Per-endpoint DTO cloning where the payload is one concept (an inbound user message and its per-turn options). Any new per-turn option (like the recently added max_turns, present in all four) must be added in four places; the identical Start/Queue pair exists only to give two Restate handlers distinct type names. MOA is pre-prod, so there is no wire-compat reason to keep the duplicates.

**Simpler alternative.** Define one `TurnMessage { user_message, attachments, model, contact, max_turns }` and `#[serde(flatten)]` it into RunTurnRequest and PendingMessage; use it directly (or one MessageRequest type) for both Session/start_turn and Session/queue_message, and collapse the two response types into one `{ queued, turn_id }`.

**Side effects / what to watch.** Handler signatures in the moa-orchestrator Session VO and edge client callers change; JSON stays identical with flatten (or may break freely pre-prod). Tests naming the removed types need renames.

**Value of simplifying.** One payload definition instead of four keeps per-turn options from drifting between the start, queue, pending, and run paths; fewer wire types to document and mirror in clients.

**Adversarial verifier: ✅ CONFIRMED.** `StartTurnRequest` and `QueueMessageRequest` are field-identical (`moa-core/src/wire/turn.rs:167`, `:195`), and `PendingMessage`/`RunTurnRequest` repeat the same message/options fields plus workflow state. `queue_message` simply converts into `StartTurnRequest` and renames the response field. Preserving JSON field names is the main side effect.

---

### 70. Agent policy mode enums duplicated byte-for-byte between moa-artifacts and moa-core, bridged by match-mappings in the resolver

**Area:** auth / agents / contacts / scoring / experiments
effort: **medium** · finder confidence: **high** · ~LOC removable: **~120**

**Locations**

- `crates/moa-agents/src/resolver.rs (lines 373-415: knowledge_policy_from_definition, skill_policy_from_definition, tool_policy_from_definition)`
- `crates/moa-artifacts/src/agent.rs (lines 117-123 KnowledgeScopeMode, 156-166 SkillPolicyMode, 213-221 ToolPolicyMode)`
- `crates/moa-core/src/types/agent.rs (lines 78-99 AgentSkillPolicyMode/AgentKnowledgeScopeMode, 171-179 AgentToolPolicyMode)`

**What it is.** The artifact-document policy enums (SkillPolicyMode, ToolPolicyMode, KnowledgeScopeMode) and the runtime-snapshot enums (AgentSkillPolicyMode, AgentToolPolicyMode, AgentKnowledgeScopeMode) are identical variant-for-variant, down to the same doc comments. moa-agents' resolver converts each with an exhaustive 1:1 match, and the surrounding *_from_definition functions mostly clone fields into equally-shaped Agent* structs (the only real logic is sorted_unique normalization and guardrail model fallback).

**Why it may be over-engineered.** moa-artifacts already depends on moa-core, so the definition types can use the moa-core enums directly; the mirror exists only to keep two names for one concept. Every new policy mode must be added in two enums plus a match arm, and forgetting the match arm is a compile-time-silent semantic risk only because the variants happen to align today.

**Simpler alternative.** Delete the moa-artifacts copies and have AgentDefinition's policy structs use the moa-core enums (AgentSkillPolicyMode etc., or rename them once to neutral names). The resolver's mapping functions shrink to just normalization (sorted_unique, guardrail fallback), with the mode fields passed through untouched.

**Side effects / what to watch.** Artifact document serde output for these fields is unchanged (same snake_case variants), so stored artifact JSON stays compatible; imports across moa-artifacts/moa-agents update. Struct-level mirrors (ModelPolicy vs AgentModelPolicy) can optionally follow but carry the normalization distinction.

**Value of simplifying.** Removes ~120 lines of mirrored enums and match-mapping and eliminates the two-place edit for every future policy mode.

**Adversarial verifier: ✅ CONFIRMED.** Artifact and runtime policy mode enums are variant-equivalent with matching serde forms (`moa-artifacts/src/agent.rs`, `moa-core/src/types/agent.rs`), and resolver mappings are 1:1 for those modes. `moa-artifacts` already depends on `moa-core`, so using core enums directly or renaming once to neutral names is viable without stored JSON shape changes.

---

### 72. AsyncAuthzProvider::poll_decision is a trait method with zero production callers; approvals resolve exclusively via awakeables

**Area:** auth / agents / contacts / scoring / experiments
effort: **small** · finder confidence: **high** · ~LOC removable: **~70**

**Locations**

- `crates/moa-core/src/traits/auth.rs (line 207)`
- `crates/moa-auth/providers/src/builtin_authz.rs (lines 73-78)`
- `crates/moa-auth/auth0/src/ciba.rs (lines 210-215, plus the force/resolve_awakeable parameter split in poll_approval at 271-281)`

**What it is.** The AsyncAuthzProvider trait exposes poll_decision(&ApprovalHandle). The builtin provider implements it as a stub that always returns Ok(None); the CIBA provider implements it by threading force/resolve_awakeable flags through its internal worker. Grepping the workspace, no production code ever calls .poll_decision() — the orchestrator does not even hold an ApprovalHandle; decisions come back solely through the Restate awakeable each provider resolves itself. The only caller is one CIBA db test.

**Why it may be over-engineered.** Speculative API surface for a pull-based decision model that the architecture (awakeable push) made obsolete. It forces every provider to implement a method that is either a lie (builtin's Ok(None)) or extra plumbing (CIBA's force flag and resolve_awakeable=false path exist only to serve this method).

**Simpler alternative.** Delete poll_decision from the trait and both impls; in ciba.rs collapse poll_approval's force/resolve_awakeable parameters (recovery loop and per-approval poller both always resolve the awakeable). Rewrite the one test against tick/recovery behavior or the DB row state.

**Side effects / what to watch.** One CIBA db test needs rework; ApprovalHandle can likely also lose provider_specific later (only constructed, never read in production), shrinking the trait further.

**Value of simplifying.** Removes a dead code path from the human-approval security surface and simplifies the CIBA worker's branching; providers implement only what actually runs.

**Adversarial verifier: ✅ CONFIRMED.** `AsyncAuthzProvider::poll_decision` has no production caller; builtin returns `Ok(None)`, Auth0 CIBA uses it only for a non-awakeable poll branch, and load-bearing flows resolve awakeables directly. Delete the method, collapse the CIBA force/resolve branch, and rewrite the one DB test around recovery/row state.

---

### 73. Entire Reporter subsystem (trait, TerminalReporter, ReporterOptions, build_reporters) has no production consumer

**Area:** eval crates
effort: **small** · finder confidence: **high** · ~LOC removable: **~560**

**Locations**

- `crates/moa-eval/src/reporter.rs`
- `crates/moa-eval/src/reporters/mod.rs`
- `crates/moa-eval/src/reporters/terminal.rs`
- `crates/moa-eval/src/reporters/json.rs`
- `crates/moa-eval/src/lib.rs:17-19`
- `crates/moa-eval/tests/eval_offline/reporters.rs`

**What it was.** moa-eval shipped a full pluggable reporting layer: an async `Reporter` trait, a `TerminalReporter` (327 lines with ANSI color handling, verbose per-case rendering, a status matrix, truncation helpers), a `JsonReporter`, a `ReporterOptions` struct with an `is_terminal()` default, and a `build_reporters()` factory that parsed CLI-style spec strings ("terminal", "json:<path>") into `Vec<Box<dyn Reporter>>` with a terminal fallback.

**Why it was over-engineered.** There is no CLI binary in moa-eval and no caller anywhere in the workspace: grepping the whole repo, `build_reporters`, `TerminalReporter`, and `ReporterOptions` were referenced only by moa-eval's own lib.rs re-exports; `JsonReporter` was used once, in tests/eval_offline/reporters.rs, a test that only tested the reporter itself. The two real consumers of eval runs (the orchestrator `Eval` service and `skill_regression`) serialize `EvalRun` themselves and never touch this layer. docs/16-evaluation.md never mentions it. This was speculative machinery for a CLI that does not exist — dynamic dispatch, a spec-string mini-language, and terminal rendering with zero users.

**Implemented simplification.** Deleted src/reporter.rs, src/reporters/ (all three files), the lib.rs re-exports, and tests/eval_offline/reporters.rs. If a JSON dump of an EvalRun is ever needed, `serde_json::to_writer_pretty(file, &run)` at the call site is the whole feature (EvalRun already derives Serialize).

**Side effects / what to watch.** None at runtime — nothing calls it. One tautological offline test is deleted with it. If a human-readable eval CLI is built later, the terminal rendering would need to be rewritten, but by then the desired output format will be known.

**Value of simplifying.** ~560 lines deleted, one trait and one factory/spec-string mini-language removed from the public API, smaller compile surface for every crate that depends on moa-eval (orchestrator under internal-eval-runner).

**Adversarial verifier: ✅ CONFIRMED.** `Reporter`, `TerminalReporter`, `JsonReporter`, `ReporterOptions`, and `build_reporters` are defined/re-exported in `moa-eval`, and targeted search found no workspace caller beyond reporter modules and their self-test. Deletion proposal holds.

**Implementation status: ✅ DONE.** Current code has no reporter module, reporter reexports, or reporter offline test. Verification passed with `cargo check -p moa-eval --all-targets --locked` and `cargo test -p moa-eval --test eval_offline --locked`.

---

### 74. PairwiseLlmJudge and the AnswerJudge trait are fully unwired — the production runner only ever calls DeterministicJudge::judge_sync directly

**Area:** eval crates
effort: **small** · finder confidence: **high** · ~LOC removable: **~420**

**Locations**

- `crates/moa-eval/src/memory_eval/judge.rs:90-97`
- `crates/moa-eval/src/memory_eval/judge.rs:150-155`
- `crates/moa-eval/src/memory_eval/judge.rs:246-354`
- `crates/moa-eval/src/memory_eval/judge.rs:413-489`
- `crates/moa-eval/src/memory_eval/runner/mod.rs:1438-1476`
- `crates/moa-eval/tests/eval_offline/memory_eval_judge.rs`

**What it is.** judge.rs defines an async `AnswerJudge` trait with two impls (DeterministicJudge, PairwiseLlmJudge). PairwiseLlmJudge does A/B + B/A double-order LLM judging with a JSON-schema response format, verdict-token fuzzy parsing, `PairwiseOrder`/`JudgeVerdict` enums, a `PairwiseWinner` outcome field, and `JudgeInput.baseline_answer`/`with_baseline_answer` plumbing.

**Why it may be over-engineered.** The only production call site is runner/mod.rs:1468, which calls `DeterministicJudge::new().judge_sync(...)` concretely — never through the trait. For the open-ended probe types (MultiHop, PreferenceApplication) the runner simply returns `Ok(None)` (`deterministic_judge_supports` at runner/mod.rs:1471) instead of invoking the pairwise judge, and `pairwise_winner` is not plumbed into ProbeResult or any report (grep of metrics.rs/report.rs finds no pairwise field). The DeterministicJudge error message even advertises "use PairwiseLlmJudge" for a path no lane can reach. Outside judge.rs, the pairwise types appear only in mod.rs re-exports and in tests/eval_offline/memory_eval_judge.rs, which tests the judge in isolation. docs/eval/memory-eval-pipeline.md does not reference pairwise judging. This is a trait + a full LLM-judge implementation kept for a consumer that was never built.

**Simpler alternative.** Delete PairwiseLlmJudge, the AnswerJudge trait, PairwiseWinner/PairwiseOrder/JudgeVerdict, JudgeOutcome.pairwise_winner, JudgeInput.baseline_answer/with_baseline_answer, the pairwise prompt/response-format helpers, and the pairwise halves of tests/eval_offline/memory_eval_judge.rs. Keep DeterministicJudge as a plain struct with judge_sync (drop the async trait impl). If an LLM judge is added later for the live lane, reintroduce it with the actual wiring.

**Side effects / what to watch.** Open-ended probes stay unscored, exactly as today (the runner already skips them). The deterministic judge's error message needs rewording. If the team genuinely intends to wire pairwise judging into the nightly live lane soon, this is premature deletion — but nothing in code or docs shows that plan.

**Value of simplifying.** ~420 lines deleted (judge.rs shrinks by ~300, plus ~120 lines of self-referential tests), one trait and one dormant LLM-call path removed, and the judge module stops implying a capability the eval pipeline does not have.

**Adversarial verifier: ✅ CONFIRMED.** `AnswerJudge` and `PairwiseLlmJudge` are implemented and re-exported, but the runner calls `DeterministicJudge::new().judge_sync(...)` directly and returns `Ok(None)` for open-ended probes. Delete the unwired trait/LLM judge path unless a concrete runner integration is added first.

**Implementation status: superseded by coverage.** User chose to keep pairwise judging. The code now retains `PairwiseLlmJudge` and `AnswerJudge`, and `crates/moa-eval/tests/eval_offline/memory_eval_judge.rs` includes a `PAIRWISE_JUDGE_EVAL_SET` that exercises swapped-order candidate win, baseline win, no-agreement, invalid verdict, and closed-form rejection paths. Mutation verification temporarily broke the B/A verdict mapping and the eval set failed on the candidate-wins case as expected.

---

### 77. Pentest support is library code for exactly one test binary, with drifted env knobs that CI sets under names the code no longer reads

**Area:** eval crates
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~630 moved + ~70 deleted**

**Locations**

- `crates/moa-eval/src/pentest/fixtures.rs`
- `crates/moa-eval/src/pentest/mod.rs`
- `crates/moa-eval/src/pentest/fixtures.rs:484-496`
- `crates/moa-eval/pentest_report.json`
- `.github/workflows/deploy.yml:112-115`

**What it is.** src/pentest/fixtures.rs (627 lines: PentestStack, attack seeding, a REPORT_LOCK-guarded read-merge-write JSON report file in the CWD, scoped-connection helpers) is compiled into the moa-eval library but consumed only by tests/cross_tenant_pentest_db_memory.rs. Its scale knobs read `MOA_PENTEST_TENANT_COUNT` / `MOA_PENTEST_FACTS_PER_TENANT`, while .github/workflows/deploy.yml sets `MOA_PENTEST_WORKSPACE_COUNT` / `MOA_PENTEST_FACTS_PER_WORKSPACE` — names the code never reads, so the CI-configured 100x20 scale is silently ignored and defaults run instead. A 67-line generated run artifact, pentest_report.json, is committed at the crate root.

**Why it may be over-engineered.** The repo already has the right pattern for single-consumer test support: tests/memory_eval_support/ modules inside the test harness. Putting pentest machinery in src/ makes every moa-eval consumer (including the orchestrator under internal-eval-runner) compile attack-seeding code, and the public no-notice API invited the env-var rename drift that now defeats the CI configuration. The committed report artifact is stale output living in the source tree.

**Simpler alternative.** Move fixtures.rs into tests/pentest_support/ (mirroring tests/memory_eval_support/) and delete src/pentest/. Pick one env-var naming (the CI ones, since deploy.yml already uses them) and make the code read exactly those. Delete the committed pentest_report.json from the crate root (CI already redirects via MOA_PENTEST_REPORT to the workspace).

**Side effects / what to watch.** Mostly a move rather than a deletion; imports in the pentest test change from `moa_eval::pentest::fixtures` to a local module. Fixing the env names changes CI pentest scale from the silent defaults back to the intended 100 tenants x 20 facts, which will lengthen that CI job — that is the configured behavior being restored, but worth flagging.

**Value of simplifying.** ~630 lines leave the library compile path, one env-knob drift bug (CI scale silently ignored) is eliminated, and a stale generated artifact leaves the repo.

**Adversarial verifier: ✅ CONFIRMED.** Pentest support is exported from the library and consumed by exactly one test binary; code reads tenant-named env vars while CI sets workspace-named vars. Move fixtures under the test harness and normalize env names.

---

### 78. Duplicated Restate ingress HTTP client and session-bootstrap sequence in moa-loadtest vs moa-test-support

**Area:** loadtest / test-support / xtask / scripts
effort: **medium** · finder confidence: **high** · ~LOC removable: **~300**

**Locations**

- `crates/moa-loadtest/src/backend.rs:296-558`
- `crates/moa-loadtest/src/backend.rs:71-128`
- `crates/moa-test-support/src/orchestrator_fixture/client.rs:1-266`
- `crates/moa-test-support/src/orchestrator_fixture.rs:434-496`

**What it is.** moa-loadtest's private RemoteHttpClient/RemoteSessionHandle (backend.rs) is a near-verbatim copy of moa-test-support's TestApiClient/TestSessionHandle: identical /restate/call POST helpers (post_call, post_call_with_idempotency, post_empty_call, post_void), identical decode_response, identical x-moa-* identity-header logic (apply_auth vs authed), identical SessionStore endpoints (create_session, init_session_vo, append_event, get_session, get_events), and an identical await_turn_outcome snapshot-polling loop. RemoteTarget::start_session also re-implements the exact create_session -> grant participant tuple -> append SessionCreated -> init_session_vo sequence that IsolatedTest::create_session performs.

**Why it may be over-engineered.** Two hand-maintained copies of the same wire client exist in the same workspace for the same Restate ingress surface. They are already drifting: TestSessionHandle grew a status() handler the loadtest copy lacks, while the loadtest copy grew timeout classification (RemoteHttpError::Timeout) and per-request idempotency keys the fixture copy lacks. Any ingress protocol or header change must now be fixed twice, and a bug fixed in one client silently persists in the other. MOA is pre-prod, so there is no compatibility reason to keep two clients.

**Simpler alternative.** Keep one ingress client. Either (a) let moa-loadtest depend on moa-test-support and delete RemoteHttpClient/RemoteSessionHandle plus the start_session bootstrap (moa-loadtest is itself dev tooling, never deployed; add it to the exemption list in xtask's audit_moa_test_support_dev_dependency_only the same way crates/xtask is already exempted), or (b) move TestApiClient/TestSessionHandle into a tiny shared test-client module both crates use. Fold the loadtest-specific bits (request timeout, Timeout error kind) into the single client as options.

**Side effects / what to watch.** The xtask path audit rule 'moa-test-support only from dev-dependencies' needs one glob exemption or the client needs a new home; TurnFailureKind classification in the loadtest must be rebuilt on top of the shared client's error type. No production code is affected — both crates are test/load tooling.

**Value of simplifying.** Deletes ~260-300 lines of copy-pasted HTTP/wire code, removes a proven drift channel between the load harness and the e2e fixture, and gives protocol changes a single edit point.

**Adversarial verifier: ✅ CONFIRMED.** `moa-loadtest` and `moa-test-support` each implement Restate ingress clients and session bootstrap sequences. Consolidation is valid if it preserves loadtest timeout/error classification and fixture `status()`; using `moa-test-support` normally requires updating the xtask dev-dependency-only audit.

---

### 82. xtask migrate-test-db is a no-op that four CI steps build a release binary to run

**Area:** loadtest / test-support / xtask / scripts
effort: **small** · finder confidence: **high** · ~LOC removable: **~35 + 4 CI steps**

**Locations**

- `crates/xtask/src/main.rs:42-49`
- `crates/xtask/src/main.rs:260-279`
- `.github/workflows/deploy.yml:218`
- `.github/workflows/long-conv-eval.yml:50`
- `.github/workflows/long-conv-eval.yml:104`
- `.github/workflows/memory-eval-nightly.yml:51`

**What it is.** cmd_migrate_test_db reads MOA_DATABASE_URL, redacts the password with a hand-rolled 20-line URL parser (redact_password, used nowhere else), and prints an informational message saying migrations happen elsewhere ('integration tests create migrated isolated schemas during bootstrap'). It performs no migration or any other side effect. Four CI workflow steps run it via `cargo run -p xtask --release -- migrate-test-db`, paying a release build of xtask and its dependency tree to print one line.

**Why it may be over-engineered.** It is a vestigial command kept after the real migration work moved into test bootstrap. A CI step that compiles a Rust release binary to echo a sentence is pure overhead and a misleading signal that something is being migrated.

**Simpler alternative.** Delete cmd_migrate_test_db and redact_password from xtask, and delete the four workflow steps (or replace each with a one-line `test -n "$MOA_DATABASE_URL"` shell check if the env-presence assertion is the part worth keeping).

**Side effects / what to watch.** None functional; the workflows lose an early loud failure when MOA_DATABASE_URL is unset, which the suggested shell check preserves for free.

**Value of simplifying.** Removes dead code plus four CI steps (each a cargo release build when uncached) and eliminates a misleading 'migrate' name.

**Adversarial verifier: ✅ CONFIRMED.** `xtask migrate-test-db` only reads/redacts `MOA_DATABASE_URL`, prints a message, and returns; four workflow steps build/run it as a no-op probe. Delete it or replace it with a shell env check.

---

### 83. inspectable_files: dead configurable workspace-root branch only ever called with None

**Area:** loadtest / test-support / xtask / scripts
effort: **small** · finder confidence: **high** · ~LOC removable: **~55**

**Locations**

- `crates/moa-loadtest/src/plan.rs:23-65`
- `crates/moa-loadtest/src/runner.rs:229`
- `crates/moa-loadtest/src/config.rs:1-8`

**What it is.** inspectable_files(workspace_root: Option<&Path>) carries a Some(root) branch with two candidate-file arrays, async tokio::fs::try_exists probing via first_existing_relative_path, and fallback chaining — but its single call site (runner.rs:229) passes None, so the function always returns the two hardcoded strings "Cargo.toml" and "docs/02-brain-orchestration.md". The whole probing branch, first_existing_relative_path, and the async/Result signature are dead. (Same pattern in miniature: config.rs exists solely to wrap MoaConfig::load() in load_config().)

**Why it may be over-engineered.** Speculative configurability: the parameter, the async filesystem probing, and the fallback candidate lists serve a caller that does not exist. The prompts these filenames feed are consumed by a scripted mock provider anyway, so probing the real filesystem adds nothing even in principle.

**Simpler alternative.** Replace InspectionFiles construction with two consts (or a Default impl) and delete the Option parameter, the candidate arrays, and first_existing_relative_path; runner.rs drops an await and a `?`. Fold config.rs's one-line wrapper into the harness call site while there.

**Side effects / what to watch.** None; behavior is bit-identical today. The unit-test helper in tests.rs already builds InspectionFiles literally.

**Value of simplifying.** Deletes ~50 lines and one async filesystem dependency from run setup; removes a knob nobody can turn.

**Adversarial verifier: ✅ CONFIRMED.** `inspectable_files(Some(root))` exists, but the only runtime caller passes `None`; tests construct `InspectionFiles` directly, and `config.rs` only wraps `MoaConfig::load()`. Delete the configurable-root branch and fold the wrapper.

---

### 85. IngestionObserver trait + pipeline type parameter with one zero-sized impl used everywhere, including tests

**Area:** cross-cutting: single-impl abstractions
effort: **small** · finder confidence: **high** · ~LOC removable: **~70**

**Locations**

- `crates/moa-knowledge/src/observability.rs:191-205`
- `crates/moa-knowledge/src/ingestion.rs:263-269`
- `crates/moa-orchestrator/src/services/knowledge/ingest.rs:174`

**What it is.** KnowledgeIngestionPipeline is generic over `O: IngestionObserver`, threading an Arc'd observer through ingestion. The only implementation anywhere is MetricsIngestionObserver, a zero-sized struct whose record_step just emits tracing spans and metrics counters; every production construction site AND every test (knowledge_db_memory harness) passes `Arc::new(MetricsIngestionObserver)`.

**Why it may be over-engineered.** A trait + generic parameter whose sole purpose is dependency injection is dead weight when even the tests inject the real thing — there is no seam being exercised. The trait signature also carries `sync_run_uid`/`object_uid` parameters that the only impl ignores (both underscored). The 'sink for redacted progress' abstraction is a plain function call to `metrics!`/`tracing!` wearing an async_trait costume.

**Simpler alternative.** Delete the IngestionObserver trait and the `O` type parameter; keep record_step as a free function (or inherent fn) in observability.rs and call it directly from the pipeline. Drop the two ignored parameters while at it.

**Side effects / what to watch.** Pipeline type signature shrinks from 5 to 4 generics; test call sites drop one Arc argument. No behavior change — the same metrics/spans are emitted.

**Value of simplifying.** Removes one trait, one generic parameter on the workspace's biggest pipeline type, and an Arc allocation per pipeline; fewer moving parts in an already generic-heavy module.

**Adversarial verifier: ✅ CONFIRMED.** `IngestionObserver` has one implementation, `MetricsIngestionObserver`, and production hardcodes it; the generic parameter adds no useful seam. Replace the trait/type parameter with a direct helper or inherent function.

---

### 86. Test doubles living in production knowledge-service module, one of them dead

**Area:** cross-cutting: single-impl abstractions
effort: **small** · finder confidence: **high** · ~LOC removable: **~130**

**Locations**

- `crates/moa-orchestrator/src/services/knowledge/mod.rs:575-645`
- `crates/moa-orchestrator/src/services/knowledge/mod.rs:717-741`

**What it is.** StaticKnowledgeProviders is a full KnowledgeProviderResolver implementation (HashMap of providers + webhook verifiers, builder methods) whose own doc comment says 'used by offline service tests'; it is constructed only in tests/knowledge_service.rs. DeterministicKnowledgeCredentialStore is a KnowledgeCredentialStore impl documented as 'used by tests' that has zero references anywhere in the repo — tests define their own FakeKnowledgeCredentialStore instead.

**Why it may be over-engineered.** Production module carries ~120 lines that no production path can reach. DeterministicKnowledgeCredentialStore is outright dead code (unused pub item, so rustc never warns). StaticKnowledgeProviders doubles the resolver surface next to the real ConfigKnowledgeProviders, making the provider-resolution code look like it has two production modes when it has one. The KnowledgeProviderResolver trait itself is justified only as a test seam — which works equally well with the double defined in the test harness.

**Simpler alternative.** Delete DeterministicKnowledgeCredentialStore. Move StaticKnowledgeProviders into the tests/knowledge_service.rs support module (it only needs the pub trait, which stays). Production mod.rs keeps ConfigKnowledgeProviders as the single resolver.

**Side effects / what to watch.** tests/knowledge_service.rs gains ~90 lines of local support code (or a shared test-support module); no production behavior change.

**Value of simplifying.** Shrinks the production knowledge service by ~120 lines and removes a fake 'second implementation' that misleads readers about the provider-resolution design.

**Adversarial verifier: ✅ CONFIRMED.** `StaticKnowledgeProviders` and `DeterministicKnowledgeCredentialStore` are test/deterministic support living in production knowledge-service code; only tests construct the static provider, and tests use their own fake credential store. Move the test double to the harness and delete the dead deterministic store.

---

### 88. DeliverySink trait is pure ceremony: single impl, never used as dyn or bound, consumer names the concrete type

**Area:** cross-cutting: single-impl abstractions
effort: **small** · finder confidence: **high** · ~LOC removable: **~15**

**Locations**

- `crates/moa-messaging/src/delivery.rs:118-123`
- `crates/moa-messaging/src/delivery.rs:269`
- `crates/moa-contacts/src/repository.rs:10-375`

**What it is.** The async_trait DeliverySink defines one method, deliver(). Its only implementation (workspace-wide, tests included) is ProviderDeliverySink. There is no `dyn DeliverySink` and no `T: DeliverySink` bound anywhere; moa-contacts constructs `ProviderDeliverySink::from_env(...)` directly and imports the trait only for method resolution, and the offline tests also use ProviderDeliverySink.

**Why it may be over-engineered.** A one-method trait with one impl, no trait-object usage, and no mock is indistinguishable from an inherent method except for the extra async_trait boxing and the import ceremony at every call site. Channel plurality is already handled inside ProviderDeliverySink via feature-gated Postmark/Twilio clients, so a second sink type has no realistic role.

**Simpler alternative.** Delete the DeliverySink trait and make `deliver` an inherent async method on ProviderDeliverySink; drop the trait imports in moa-contacts and the offline test.

**Side effects / what to watch.** None beyond import cleanup; if a test double is ever wanted later, reintroducing a trait is a five-line change.

**Value of simplifying.** Removes an async_trait vtable hop and a phantom extension point from the messaging crate's public API.

**Adversarial verifier: ✅ CONFIRMED.** `DeliverySink` has one implementation, `ProviderDeliverySink`; contacts and messaging tests construct the concrete sink and import the trait only for method resolution. An inherent `deliver` method is enough.

---

### 91. slack/postmark/twilio feature triple-gating means every built binary ships without messaging, while the env surface pretends otherwise

**Area:** cross-cutting: config & feature-flag sprawl
effort: **small** · finder confidence: **high** · ~LOC removable: **~60 (or ~3050 if adapters are deleted instead)**

**Locations**

- `crates/moa-messaging/Cargo.toml (default = ["slack", "postmark", "twilio"])`
- `Cargo.toml:113 (moa-messaging default-features = false)`
- `crates/moa-orchestrator/Cargo.toml:14-16 (forwarding features)`
- `crates/moa-orchestrator/src/runtime/deps.rs:268-285 (cfg(feature="slack") construction shim)`
- `crates/moa-messaging/src/lib.rs:3-59 (cfg gates)`
- `.env.example:100-107 (MOA_MESSAGING_* vars)`

**What it is.** moa-messaging gates its Slack (slack-morphism), Postmark, and Twilio adapters (3,051 lines) behind three features that are default-on in the crate — but the workspace dependency sets default-features = false, and the orchestrator re-exposes them as forwarding features that no build enables (Dockerfile: `redis`; compose: `redis,provider-overrides`; e2e: `provider-overrides,skill-learning,redis`; CI/k8s/fly: defaults). deps.rs carries cfg/not(cfg) construction branches for the Slack adapter.

**Why it may be over-engineered.** The flag layer has zero consumers: every deployable artifact is compiled without Slack/email/SMS even though .env.example documents MOA_MESSAGING_SLACK_TOKEN etc. and docs/03/docs/10 present Slack as a core communication pillar. The crate's own default feature list is equally dead since both consumers (orchestrator, moa-contacts) disable defaults. Adapter presence is already runtime-controlled by whether tokens are configured, so the compile-time gate adds a second, contradictory switch.

**Simpler alternative.** Delete the three features from moa-messaging (compile the adapters unconditionally — reqwest is already everywhere and slack-morphism is already in the lockfile), delete the three forwarding features in moa-orchestrator, and remove the cfg branches in deps.rs and lib.rs. Runtime enablement stays exactly as it is: adapter is active only when its token env vars are set.

**Side effects / what to watch.** slack-morphism becomes an unconditional dependency of the orchestrator build (build-time cost only; no runtime change when tokens are absent). If the intent was genuinely to keep Slack out of prod binaries, that intent is currently unrealized anyway — this makes the config surface and the binary agree.

**Value of simplifying.** Removes four feature flags and a cfg shim, and fixes a real deployment wedge: today setting MOA_MESSAGING_SLACK_TOKEN on the shipped image does nothing because the code isn't in the binary.

**Adversarial verifier: ✅ CONFIRMED.** `moa-messaging` defaults adapters on, the workspace dependency disables defaults, orchestrator only forwards features, and deploy/test paths do not include them while `.env.example` still exposes messaging vars. Remove compile gates or explicitly build shipped artifacts with Slack/Postmark/Twilio features enabled.

---

### 95. Retry-After parsing, retryable-status classification, jitter, and response-body helpers are implemented independently in four crates

**Area:** cross-cutting: cross-crate duplication
effort: **medium** · finder confidence: **high** · ~LOC removable: **~250**

**Locations**

- `crates/moa-providers/src/core/retry.rs:151-290`
- `crates/moa-providers/src/core/http.rs:35-56`
- `crates/moa-messaging/src/provider_http.rs:29-61`
- `crates/moa-messaging/src/rate_limit.rs:444-483`
- `crates/moa-hands/src/adapters/http_util.rs:33-81`
- `crates/moa-knowledge/src/providers/mod.rs:152-248`
- `crates/moa-providers/src/memory_llm/client.rs:222-250`

**What it is.** Four crates each carry a crate-private HTTP helper module doing the same job: build a reqwest client with 10s connect timeout, read an error body defensively, classify retryable HTTP statuses, parse the Retry-After header, and add jitter to backoff. parse_retry_after is byte-identical in moa-providers/src/core/retry.rs:264 and moa-messaging/src/rate_limit.rs:449. is_retryable_status matches the same 5 statuses in providers and messaging/provider_http.rs. response_text is duplicated in providers retry.rs and messaging provider_http.rs. Three separate jitter implementations exist (RetryPolicy::jitter_seed clock-nanos hack, messaging with_jitter via rand, memory_llm retry_delay_with_jitter via rand).

**Why it may be over-engineered.** The copies have already drifted in ways that look accidental rather than deliberate: messaging's rate_limit.rs treats 408 as retryable while providers does not; moa-hands and moa-knowledge parse Retry-After as integer seconds only and silently ignore RFC2822 date values that the other two crates handle; providers derives 'jitter' deterministically from the wall-clock nanos while the other two use rand. All four crates already depend on moa-core and three of them build the same moa_core::MoaError::HttpStatus { status, retry_after, message }. This is textbook near-duplicate helper logic where each new provider integration re-solves the same problem slightly differently.

**Simpler alternative.** Add one moa-core module (e.g. moa_core::http_retry) hosting: parse_retry_after (full seconds+RFC2822), retry_after_delay(&HeaderMap), is_retryable_http_status(StatusCode) (pick one status set, include 408), response_text(Response), with_jitter(Duration), and the timeout-bounded client builders (streaming vs whole-request variants). Delete the four crate-private copies and their duplicated unit tests; moa-knowledge maps the shared result into its own Error enum at the boundary. Domain-specific loops (RetryPolicy+RateGuard, MessagingRateLimiter pacing) stay where they are but call the shared primitives. Note backon is already a workspace dependency (moa-lineage-sink, moa-memory-vector, moa-session) for the plain exponential loops.

**Side effects / what to watch.** Behavior unifies: hands/knowledge start honoring RFC2822 Retry-After dates, and one crate's retryable set changes by 408 (either direction). Duplicated helper unit tests in each crate get deleted or moved. No wire or migration impact; pre-prod so no compat concern. moa-knowledge keeps its own Error enum per AGENTS.md.

**Value of simplifying.** Deletes roughly 200-300 lines plus duplicated tests, but the main win is removing four independently-drifting implementations of rate-limit interpretation — a class of bug (mis-parsed Retry-After causing hammering or over-sleeping) that currently must be fixed in four places.

**Adversarial verifier: ✅ CONFIRMED.** Providers and messaging duplicate retryable-status and full Retry-After date parsing, while hands and knowledge parse only integer seconds. Shared low-level HTTP retry primitives are justified; preserve crate-specific error mapping.

---

### 96. Ephemeral test-Postgres bootstrap (URL fallback + identifier quoting + search-path pool + schema migrations) copy-pasted across ~14 test-support files

**Area:** cross-cutting: cross-crate duplication
effort: **medium** · finder confidence: **high** · ~LOC removable: **~450**

**Locations**

- `crates/moa-ocsf/tests/support.rs:1-40`
- `crates/moa-auth/auth0/tests/support.rs:1-42`
- `crates/moa-auth/auth0/tests/auth_providers_auth0_db/ciba_db.rs:277-358`
- `crates/moa-auth/authz/tests/authz_db/authz_poller_db.rs:115-200`
- `crates/moa-auth/authz/tests/authz_db/outbox_basic_db.rs`
- `crates/moa-auth/authz/tests/authz_db/require_audit_db.rs`
- `crates/moa-auth/providers/tests/auth_providers_db/api_keys_lifecycle_db.rs:99-125`
- `crates/moa-auth/providers/tests/auth_providers_db/builtin_authz_request_db.rs:92-123`
- `crates/moa-lineage/sink/tests/writer_db.rs:355-370`
- `crates/moa-lineage/audit/tests/merkle_publisher_db.rs:230-245`
- `crates/moa-db/tests/scoped_conn_rls_db.rs:20-30`
- `crates/moa-memory/lifecycle/tests/memory_lifecycle_db_memory/quality_postgres_db_memory.rs:480-495`
- `crates/moa-memory/vector/tests/memory_vector_db_memory/pgvector_store_db_memory.rs:965-985`
- `crates/moa-migrations/tests/run_idempotency_db.rs`

**What it is.** Each of these test-support files re-implements the same ~35-line fixture: read MOA_DATABASE_URL with the hardcoded compose fallback (postgres://moa_owner:dev@localhost:10040/moa — hardcoded in 18 files), generate a uuid-suffixed schema name, build a PgPoolOptions pool with an after_connect closure running set_config('search_path', ...), define a private quote_identifier, and call the crate-relevant moa_migrations::run_*_schema functions. moa-test-support/src/postgres.rs already exposes test_database_url(), DEFAULT_DATABASE_URL, and a TestDb bootstrap, and moa-test-support/src/fixtures.rs exposes a public quote_identifier, yet the copies persist.

**Why it may be over-engineered.** This is the same fixture written ~14 times with only the schema-name prefix and the list of schema migrations varying. The duplication also hardcodes the compose port 10040 in 18 places, so a compose change means a repo-wide sed. Nothing about these lanes requires per-crate implementations; every one of these test binaries already dev-depends on moa-migrations.

**Simpler alternative.** Add one shared helper — either moa_migrations::testing::migrated_pool(schema_prefix: &str, schemas: &[Schema]) -> (PgPool, String) behind a cheap feature/testing module (moa-migrations is already a dev-dependency of every offender and has no cycle risk), or extend moa-test-support::postgres for crates that can take that heavier dev-dep. It owns the env-var fallback, schema-name generation, search-path after_connect, quoting, and migration application. Each support file shrinks to a one-line call.

**Side effects / what to watch.** Pure test-support refactor; no production behavior change. Crates below moa-test-support in the graph (e.g. moa-authz, moa-session) either use the moa-migrations home or a dev-dependency cycle-free path — putting it in moa-migrations avoids the question entirely. Nextest lane structure (per AGENTS.md harness rules) is untouched.

**Value of simplifying.** Deletes ~400-500 lines of copy-paste, removes 18 hardcoded database URLs, and gives future _db test lanes a one-line bootstrap instead of a fresh copy that can drift (timeouts, max_connections, and cleanup already differ slightly between copies).

**Adversarial verifier: ✅ CONFIRMED.** Repeated URL fallback, search-path, identifier quoting, and migration setup exists across test support files, while `moa-test-support` already has `DEFAULT_DATABASE_URL`, `test_database_url`, `TestDb`, and public `quote_identifier`. A shared migrated-pool helper is a good simplification.

---

### 97. ~35 hand-rolled mock LLMProvider implementations with a 25-line ModelCapabilities/pricing blob duplicated 29 times, alongside an existing general-purpose ScriptedProvider

**Area:** cross-cutting: cross-crate duplication
effort: **large** · finder confidence: **medium** · ~LOC removable: **~500**

**Locations**

- `crates/moa-brain/tests/brain_turn_support/offline.rs (9 mock providers, 609 lines)`
- `crates/moa-brain/src/pipeline/history/test_support.rs:161`
- `crates/moa-brain/src/pipeline/compactor/test_support.rs:120`
- `crates/moa-brain/tests/brain_turn_support/artifacts.rs:24-154`
- `crates/moa-brain/tests/brain_turn_support/session_search.rs:53-178`
- `crates/moa-orchestrator/tests/orchestrator_offline/llm_gateway.rs:72`
- `crates/moa-orchestrator/tests/skill_learning_workflow.rs:383`
- `crates/moa-providers/src/adapters/scripted/mod.rs`

**What it is.** Roughly 35 of the 40 `impl LLMProvider for ...` blocks in the workspace are test mocks. Most repeat the same full ModelCapabilities struct (claude-sonnet-4-6, 200_000 context, identical TokenPricing) — `context_window: 200_000` appears 29 times across 21 files — plus a boilerplate complete() returning CompletionStream::from_response. moa-brain alone defines MockLlmProvider three separate times (offline.rs, history/test_support.rs, compactor/test_support.rs). Meanwhile moa-providers ships ScriptedProvider (712 lines), a keyed scripted provider supporting tool-call loops, which sibling tests in the same crate already use (crates/moa-brain/tests/stable_prefix_db_memory.rs, brain_turn_cache_replay_db_memory.rs, skill_package_materialization_db_memory.rs).

**Why it may be over-engineered.** The mocks fall into three repeated shapes — fixed-text responder, request-capturing responder, and scripted tool-call loop — all of which ScriptedProvider or a tiny shared capturing wrapper can express. Because each mock inlines the capabilities blob, any change to ModelCapabilities (a new field) touches ~29 sites; that already forces mechanical edits (the struct has 10+ fields each mock must fill). Three identically-named MockLlmProviders inside one crate is drift waiting to happen.

**Simpler alternative.** Add to moa-test-support (or a moa-providers test-util feature): (1) pub fn anthropic_test_capabilities() -> ModelCapabilities as the single fixture blob; (2) a CapturingProvider wrapper (Arc<Mutex<Vec<CompletionRequest>>> around any inner LLMProvider); (3) migrate fixed-text and tool-loop mocks to ScriptedProvider keyed scripts. Keep only genuinely behavioral mocks (e.g. mid-stream failure injection) as bespoke impls built on the shared capabilities fixture.

**Side effects / what to watch.** Large test-only migration; each converted test needs re-verification that its assertion still exercises the same path (some mocks assert on exact request ordering or leak canaries that must map onto ScriptedProvider script keys). No production code changes. Risk of subtly weakening a test during conversion — do it incrementally per test file.

**Value of simplifying.** Realistically deletes 400-700 lines of mock boilerplate and collapses the 29-site capabilities blob to one function, so adding a ModelCapabilities field stops being a repo-wide edit. Fewer parallel mock dialects makes new test authoring converge on one recipe.

**Adversarial verifier: ✅ CONFIRMED.** Many hand-rolled `impl LLMProvider for` test providers remain, with repeated Sonnet capability blobs, while `ScriptedProvider` already exists and is used by sibling tests. Add shared capabilities/capturing helpers and migrate incrementally.

---

### 99. memory_llm::LlmChatClient re-implements retry, jitter, error classification, and provider failover inside moa-providers, parallel to the crate's own RetryPolicy/RateGuard/FailoverLLMProvider stack

**Area:** cross-cutting: cross-crate duplication
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~120**

**Locations**

- `crates/moa-providers/src/memory_llm/client.rs:93-260`
- `crates/moa-providers/src/core/retry.rs`
- `crates/moa-providers/src/core/rate_guard.rs`
- `crates/moa-providers/src/failover.rs`

**What it is.** LlmChatClient (the Cohere chat client used by moa-memory-ingest for fact extraction/merge/contradiction judging) carries its own single-retry loop (chat_with_retry), its own jitter helper (retry_delay_with_jitter — a copy of messaging's with_jitter logic), its own reqwest error mapping and per-status classification (map_reqwest_error, error_for_status), its own pacer, and its own ordered fallback chain (chat() iterating self.fallbacks on retryable errors). The same crate already owns RetryPolicy::send_gated (backoff + Retry-After + rate-guard cooldown), RateGuard, and FailoverLLMProvider (ordered candidate chain on rate-limit-class failures).

**Why it may be over-engineered.** Within one crate there are now two retry loops, two failover-chain implementations, two 429-classification tables, and two jitter functions. The memory path's requirements (non-streaming, small model, request pacing) do not need a different retry/failover algorithm — only different defaults. The bespoke stack means rate-limit lessons learned in core/retry.rs (e.g. Retry-After honored from body JSON, guard cooldown shared across concurrent calls) never reach the memory ingestion path, which hits the same Cohere endpoint.

**Simpler alternative.** Rebuild chat_once on the crate's core primitives: use core::http::build_json_http_client, wrap the send in RetryPolicy::send_gated with a small retry budget and a per-client RateGuard (replacing the hand pacer or composing with it), and express fallbacks as an ordered loop over candidates exactly once — or drop the bespoke chain and reuse the FailoverLLMProvider pattern generalized over a non-streaming call. Keep LlmChatError as the typed boundary for moa-memory-ingest (its variants are constructed but only Auth is matched externally; consider shrinking it to Auth/Retryable/Malformed).

**Side effects / what to watch.** Retry timing for memory ingestion changes (currently exactly one 200ms-jittered retry; RetryPolicy defaults are 3 retries from 1s) — tune with with_max_retries to keep ingestion latency bounded. Tests in memory_llm/client.rs and moa-memory-ingest that pin current retry behavior need updating. No API change for moa-memory-ingest if LlmChatClient's signature is preserved.

**Value of simplifying.** Deletes ~100-150 lines and removes a second, weaker retry/failover implementation from the hot rate-limited Cohere path, so cooldown/Retry-After handling is consistent between chat completions and memory extraction.

**Adversarial verifier: ✅ CONFIRMED.** `memory_llm::LlmChatClient` has its own pacer, retry loop, jitter, status mapping, and fallback vector, while the same crate has `RetryPolicy::send_gated`, `RateGuard`, and `FailoverLLMProvider`. Rebuild on shared primitives while preserving `LlmChatError` as the memory-ingest boundary.

---

## 🟡 Adjusted findings (core claim confirmed, proposal corrected)

_These findings are directionally valid, but the verifier found factual corrections, narrower safer proposals, or missing side effects._

### 24. ConsolidateDurableSteps trait layer whose only purpose is a tautological replay test

**Area:** Orchestrator — workflows & turn_driver
effort: **medium** · finder confidence: **high** · ~LOC removable: **~380**

**Locations**

- `crates/moa-orchestrator/src/workflows/consolidate.rs:221-557`
- `crates/moa-orchestrator/src/workflows/consolidate.rs:87-191`
- `crates/moa-orchestrator/tests/orchestrator_offline/replay_determinism.rs`

**What it is.** The Consolidate workflow body is split into an 11-method public `ConsolidateDurableSteps` trait (`mark_consolidation_started`, `capture_now`, `merge_duplicates`, ... `advance_consolidation_watermark`), a generic driver `run_consolidate_workflow(steps, request)`, and the single production impl `RestateConsolidateSteps` that wraps each step in `ctx.run`. The only other implementor is `RecordedConsolidateSteps` in tests/orchestrator_offline/replay_determinism.rs, which records fake step names into a recorder and replays the recorder against itself. Separately, `ConsolidateReport` carries constructors `graph_noop` and `failed` that no code calls, and fields (`records_deleted`, `relative_dates_normalized`, `orphaned_records`, `summary_records_before/after`) that every constructor hardcodes to 0/empty; `duration_ms` is hardcoded to 0 in the production `build_consolidate_report` (which also journals a pure, deterministic report construction as a durable step). The only consumer (`Tenant::consolidation_completed` in objects/tenant.rs) reads `ran_at` and logs a couple of always-zero fields.

**Why it may be over-engineered.** The trait has exactly one production implementation, and the test double does not exercise real replay: it asserts that a fake recorder replayed against its own trace is identical, which cannot catch actual nondeterminism (e.g. a `Utc::now()` outside `ctx.run` in the real impl would be invisible to the fake). The workflow body itself is a straight linear sequence with no branching worth unit-testing. Every other workflow in this crate (turn_execution, procedure_execution, skill_learning, experiment_run) achieves the same durability by calling `ctx.run` inline without a steps trait. Note the twin pattern in knowledge_sync_ingestion.rs IS justified — its body has a paging/prune/failure-staging loop exercised by two real test impls — which highlights that consolidate's copy is cargo-culted structure, not needed. The dead report fields/constructors are speculative surface with no readers.

**Simpler alternative.** Inline the step bodies directly in `Consolidate::run` as named `ctx.run` closures (exactly what `RestateConsolidateSteps` already contains), delete the `ConsolidateDurableSteps` trait, `run_consolidate_workflow`, the `RestateConsolidateSteps` struct, and the replay_determinism.rs test file. Drop `ConsolidateReport::graph_noop`/`::failed` and the always-zero fields, and build the report with a plain function call instead of a journaled step (it is pure given already-journaled inputs).

**Side effects / what to watch.** Loses the offline 'replay determinism' test, which currently pins only step ordering of a mock; the top-of-file nondeterminism audit comment can move into consolidate.rs. `ConsolidateReport` is a Restate wire payload to Tenant VO — pre-prod, so shrinking it is a permitted clean break; the tenant log line drops two always-zero fields.

**Value of simplifying.** Deletes ~350-400 lines and one abstraction layer (public trait + generic driver + async_trait indirection) from a core background workflow; new readers see one linear durable function instead of three coupled artifacts.

**Adversarial verifier: 🟡 ADJUSTED.** Verified against real code. (1) Factual accuracy: mostly right. crates/moa-orchestrator/src/workflows/consolidate.rs defines ConsolidateDurableSteps (12 methods, not 11 — lines 223-299), generic run_consolidate_workflow (302-337, a straight linear sequence with one Option match, no loops), and RestateConsolidateSteps (339-557) as the sole production impl; the only other implementor is RecordedConsolidateSteps in tests/orchestrator_offline/replay_determinism.rs (confirmed via graphify BFS and workspace grep — no other DurableSteps traits exist except knowledge_sync_ingestion.rs, which the claim correctly concedes is justified: it has a real paging loop and two genuine test impls, FakeKnowledgeSyncIngestionSteps and DbKnowledgeAutoSyncSteps in tests/knowledge_service.rs at lines 2314 and 2528). graph_noop and failed constructors have zero callers anywhere in the workspace (grep hit only their definitions). records_deleted/relative_dates_normalized/orphaned_records/summary_records_before/after are hardcoded 0/empty in all three constructors, and duration_ms is hardcoded 0 in production build_consolidate_report (consolidate.rs:506). (2) Test tautology: slightly overstated but directionally right. The Recorder (tests/support/durable_step_replay_recorder.rs) does re-canonicalize fresh step INPUTS during replay while returning journaled outputs, so body-level nondeterminism flowing into a recorded invoke input (e.g. replacing steps.capture_now() with raw Utc::now(), which would change report.ran_at inside the consolidation_completed invoke payload) WOULD fail assert_traces_identical — it is not literally incapable of catching anything. But the realistic risk class — Utc::now()/Uuid outside ctx.run inside the production RestateConsolidateSteps impl — is invisible to it, and the guarded body is a pure linear function, so the protected surface is minimal. (3) Hidden consumers the claimant missed: (a) tests/integration/consolidate_e2e.rs:126 asserts report.relative_dates_normalized == 0 and deserializes ConsolidateReport over HTTP — one assertion line must go; (b) record_memory_learning (consolidate.rs:563-577 and payload at 588-602) reads records_deleted/relative_dates_normalized in its skip gate and persists them in the LearningEntry payload — gate and JSON must be edited (persisted learning payload shrinks; pre-prod clean break is permitted per project policy); (c) Tenant::consolidation_completed (objects/tenant.rs:213-222) reads ran_at for rescheduling (load-bearing, keep) and logs records_deleted + duration_ms (always zero, droppable); (d) deleting replay_determinism.rs orphans tests/support/durable_step_replay_recorder.rs, the durable_step_recorder re-export block in tests/support/mod.rs:9,44-46, and tests/support/fake_clock.rs (replay_determinism.rs is their only consumer per grep), plus the `mod replay_determinism;` line in tests/orchestrator_offline.rs:12. (4) Load-bearing constraints: none found. No doc (docs/02-brain-orchestration.md, docs/engineering-discipline/) mandates a steps-trait or replay-determinism test; no nextest profile, Makefile, script, or CI file pins the test name. Un-journaling build_consolidate_report is replay-safe: ConsolidateReport::from_outcome is pure arithmetic over already-journaled inputs (ran_at from the journaled `now` step, outcome from journaled step outputs), so recomputing it on replay is deterministic. The nondeterminism-audit comment at the top of replay_determinism.rs should move into consolidate.rs as the claim proposes. The simplification is safe and genuinely simpler; the inline ctx.run bodies already exist verbatim inside RestateConsolidateSteps.

> **Revised simpler alternative:** As proposed: inline the RestateConsolidateSteps step bodies as named ctx.run closures directly in Consolidate::run; delete ConsolidateDurableSteps, run_consolidate_workflow, RestateConsolidateSteps, and replay_determinism.rs; drop ConsolidateReport::graph_noop/::failed and the always-zero fields (records_deleted, relative_dates_normalized, orphaned_records, summary_records_before/after, and duration_ms which is hardcoded 0 at the only production construction site); build the report with a plain from_outcome call instead of a journaled step (pure given journaled inputs). Additionally: keep ran_at (load-bearing for Tenant rescheduling); move the nondeterminism-audit comment from replay_determinism.rs into consolidate.rs; and delete the now-orphaned test support: tests/support/durable_step_replay_recorder.rs, tests/support/fake_clock.rs, their mod/pub-use lines in tests/support/mod.rs (lines 9, 10, 44-46), and `mod replay_determinism;` in tests/orchestrator_offline.rs:12.

> **Revised side effects:** Beyond what the claimant listed: (1) tests/integration/consolidate_e2e.rs:126 asserts relative_dates_normalized == 0 — delete that assertion (the e2e otherwise still compiles and passes; serde ignores nothing since both sides live in the same crate). (2) record_memory_learning in consolidate.rs must drop records_deleted/relative_dates_normalized from its skip gate (lines 567-568) and from the persisted LearningEntry JSON payload — persisted learning rows shrink, a permitted pre-prod break. (3) objects/tenant.rs:218-219 log line loses records_deleted and duration_ms fields (both always zero). (4) Deleting replay_determinism.rs orphans tests/support/durable_step_replay_recorder.rs and tests/support/fake_clock.rs (sole consumer) plus the durable_step_recorder alias module in tests/support/mod.rs — remove them or the shared support module accrues dead code; also remove the harness mod declaration in tests/orchestrator_offline.rs:12. (5) Nuance on lost coverage: the deleted test is not purely tautological — it would catch a future edit that sources ran_at from raw Utc::now() in the shared body (via the report payload recorded in the consolidation_completed invoke input) — but it cannot catch nondeterminism inside the production Restate impl, which is the realistic failure mode, so the lost protection is marginal. No nextest profile, script, or doc references the test name, so nothing else breaks.

---

### 25. Fully parallel Review vs Signal pipelines in ProcedureExecution

**Area:** Orchestrator — workflows & turn_driver
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~220**

**Locations**

- `crates/moa-orchestrator/src/workflows/procedure_execution.rs:197-237`
- `crates/moa-orchestrator/src/workflows/procedure_execution.rs:537-618`
- `crates/moa-orchestrator/src/workflows/procedure_execution.rs:886-1121`
- `crates/moa-core/src/wire/procedures.rs:135-177`

**What it is.** Blocked procedure nodes come in two kinds (review, wait-signal) and the code maintains two complete parallel paths for them: two shared handlers (`decide_review`, `signal`), two promise-key namespaces (`review:{node}` / `signal:{node}`), a `BlockedNodeKind` enum whose methods only pick between the two string prefixes and step names, two near-identical `select!` arms in `await_blocked_node_resolution` (differing only in promise payload type), two validate functions (`validate_procedure_review_decision` / `validate_procedure_signal`, ~60 lines each with identical load-run/terminal-check/node-lookup skeletons), two persist functions (`persist_procedure_review_resolution` / `persist_procedure_signal_resolution`, sharing ~40 lines of identical preamble), and two wire response structs (`ProcedureReviewDecisionResponse`, `ProcedureSignalResponse`) with byte-identical field sets {run_id, accepted, status, current_node_id}.

**Why it may be over-engineered.** The genuine difference between the two kinds is tiny: a review resolution can branch approved/rejected while a signal always completes the node with a payload, and the two service entry points carry different authz (tenant-admin vs tenant-operator). Everything else — promise plumbing, run loading, terminal checks, blocked-node membership validation, node-run persistence, response shape — is duplicated per kind. A `ProcedureBlockedNodeResolution` enum already exists internally but is only used to re-split into the two duplicate persist functions. Restate does not force this: one promise per blocked node with an enum payload journals identically.

**Simpler alternative.** Use one promise namespace `blocked:{node_id}` carrying `Json<ProcedureBlockedNodeResolution>`; keep `decide_review`/`signal` as thin shared handlers (they must stay separate for authz) that both resolve that promise. Merge the two validate functions into one `validate_blocked_node_resolution(run, node_id, expected_kind)` and the two persist functions into one that matches on the resolution enum for the approved/rejected/signal branches. Collapse the two wire response structs into one `ProcedureBlockedNodeResponse` (pre-prod wire break is allowed). Delete `BlockedNodeKind` and the duplicated select! arms.

**Side effects / what to watch.** Wire shape of decide_review/signal responses changes (identical fields, new shared type name) — edge/Skills service and any tests naming the old types need mechanical renames. In-flight procedure runs would have stale promise keys, acceptable pre-prod.

**Value of simplifying.** Deletes ~200-250 lines and removes an entire duplicated control path in the most intricate workflow file; future blocked-node kinds (e.g. timers) become one enum variant instead of a fourth parallel pipeline.

**Adversarial verifier: 🟡 ADJUSTED.** Factual check (all cited files read): the duplication is real. crates/moa-orchestrator/src/workflows/procedure_execution.rs has BlockedNodeKind (L197-232) whose three methods only pick between "review:"/"signal:" prefixes (L1493-1498) and per-kind journal step names; await_blocked_node_resolution (L537-594) has two select! branches that differ only in the promise payload type and both funnel into the already-shared persist_blocked_node_resolution_step (L596-618), which immediately re-splits ProcedureBlockedNodeResolution (L234-237, its only use) into persist_procedure_review_resolution (L1012-1072) and persist_procedure_signal_resolution (L1074-1121) sharing a ~35-line identical preamble (load_run, terminal check, load_definition, state parse, blocked_nodes kind check). validate_procedure_review_decision (L887-946) and validate_procedure_signal (L949-1010) share the load-run/node-lookup skeleton. ProcedureReviewDecisionResponse and ProcedureSignalResponse (moa-core/src/wire/procedures.rs L135-145, L167-177) have byte-identical field sets {run_id: Uuid, accepted: bool, status: String, current_node_id: Option<String>}. Hidden consumers: only crates/moa-orchestrator/src/services/skills.rs (L248-303, which does carry different authz: Relation::Admin vs Relation::Operator, as the claim said), tests (tests/procedure_execution_support/review_signal.rs, experiment_procedure_e2e.rs, experiment_trial_run_e2e.rs), and the edge (crates/moa-edge/src/routes/artifacts.rs L40-44) which only path-translates /v1/skills/signal|decide-review to /Skills/* and passes JSON through, so identical-field struct renames don't touch it. No doc pins the promise key names (docs/02-brain-orchestration.md L323 only names Skills/decide_review, which survives). Restate does not force two namespaces: one durable promise `blocked:{node_id}` with an enum payload journals deterministically; there is no per-kind determinism, performance, or PII constraint. So the over-engineering claim stands. However two corrections are needed. (1) Factual slip: the validate "terminal-checks" are NOT identical — review validation rejects unless run.status == PendingReview exactly (L899), while signal validation rejects only terminal statuses (L961-964), because persist_blocked_request (L1293-1302) sets run status PendingReview for review nodes but keeps Running for wait-signal nodes; the workflow shared handlers likewise hardcode different response statuses (L418 PendingReview vs L439 Running). A merged validate must parameterize the status gate per kind, not just the blocked_nodes kind check. (2) Missed side effect: with a single promise per node, a wrong-kind direct call to the internal workflow handler changes from inert (it resolves an unawaited `signal:{node}` promise nobody selects on) to consuming the node's only durable promise with a mismatching variant, which the persist kind-check (L1040/L1102) turns into a TerminalError — durable promises resolve once, so the blocked node cannot be re-awaited. This is defense-in-depth only (the Skills service validate is the real contract gate and Restate ingress is internal), so it does not refute the claim, but it must be listed. Also ProcedureBlockedNodeResolution currently has no serde derives (L234) and would need them, and the per-kind journal step names (procedure_review_resolution_N / procedure_signal_resolution_N) merging means in-flight runs break on step names as well as promise keys — acceptable pre-prod per repo policy (no backwards compat).

> **Revised simpler alternative:** Same as proposed, with two refinements: (a) the merged validate_blocked_node_resolution(run, node_id, expected_kind) must keep a per-kind status gate — Review rejects when run.status != PendingReview, Signal rejects only Completed/Failed/Cancelled — because signal-blocked runs sit in Running while review-blocked runs sit in PendingReview; the two thin workflow shared handlers likewise keep their per-kind hardcoded response status (PendingReview vs Running). (b) Add Serialize/Deserialize to ProcedureBlockedNodeResolution (it currently has no derives) so it can be the Json promise payload. Everything else in the proposal (single blocked:{node_id} promise, one select! path deleting BlockedNodeKind, merged persist matching on the enum for approved/rejected/signal, one ProcedureBlockedNodeResponse wire struct) is sound; the edge is unaffected because it only path-translates JSON with identical field sets.

> **Revised side effects:** In addition to the claimed side effects (wire type rename touching skills.rs service and tests in tests/procedure_execution_support/review_signal.rs, experiment_procedure_e2e.rs, experiment_trial_run_e2e.rs; stale promise keys in in-flight runs): (1) in-flight runs also break on the merged journal step names (procedure_review_resolution_N/procedure_signal_resolution_N and the cancel step names collapse), acceptable pre-prod; (2) a robustness regression for out-of-contract callers: today a wrong-kind direct call to the workflow-level ProcedureExecution/decide_review|signal handler resolves an unawaited promise and is inert, whereas with a single promise per node it consumes the node's only resolution promise and the persist kind-check turns it into a TerminalError, permanently failing the blocked node (durable promises resolve once). Normal flows are unaffected because Skills-service validation gates kind before resolving, and Restate ingress is internal-only, but this should be a conscious acceptance.

---

### 26. Hand-rolled NER returns rich labeled spans that no consumer reads; production planner ships a 12-line MOA-dev gazetteer

**Area:** moa-brain (context pipeline)
effort: **small** · finder confidence: **high** · ~LOC removable: **~180**

**Locations**

- `crates/moa-brain/src/planning/ner.rs:26-131`
- `crates/moa-brain/src/planning/planner.rs:155-184`
- `crates/moa-brain/src/planning/planner.rs:354`
- `crates/moa-brain/assets/ner-gazetteer.txt`
- `crates/moa-brain/src/pipeline/memory.rs:209`

**What it is.** planning/ner.rs is a 398-line extractor producing NerSpan { start, end, text, label } with a 6-variant NerLabel enum (Person, Org, Product, Concept, Place, Other). The only production consumer is QueryPlanner::plan, which maps spans to `span.text` strings for a batched seed lookup and passes spans to infer_label_hint(text, _spans) — which ignores them entirely (planner.rs:354). The byte offsets are used only internally for dedup ordering. Three label variants (Person, Org, Place) are never constructed anywhere, and no code outside ner.rs ever reads `label`, `start`, or `end`. The default gazetteer baked into the production retrieval planner (memory.rs:209 uses QueryPlanner::new()) is a 12-entry list of MOA's own dev vocabulary ("auth service", "postgres", "restate", "fly.io", ...), applied to every tenant's queries.

**Why it may be over-engineered.** It is an entity-recognition API shaped for a future consumer that does not exist: a coarse label taxonomy nobody matches on, offsets nobody reads, and a struct/enum surface for what is functionally 'extract candidate seed strings from a query'. The bundled dev gazetteer is a test fixture living in the production path.

**Simpler alternative.** Change the extractor to return Vec<String> of candidate seed terms (gazetteer hits, relation targets, quoted spans, code-like tokens, noun groups), deleting NerSpan, NerLabel, the offset bookkeeping in push_span/push_tokens/dedupe_spans, and the unused _spans parameter of infer_label_hint. Keep with_gazetteer(String iterator) since moa-eval's golden_eval injects corpus aliases through it, but make the production default gazetteer empty (or move ner-gazetteer.txt into the eval that needs it).

**Side effects / what to watch.** golden_eval and hybrid_retrieval_db_memory construct planners explicitly and keep working via with_gazetteer; ner.rs unit tests asserting labels are rewritten to assert extracted strings. Removing the default gazetteer slightly changes seed candidates for queries containing those 12 dev terms — memory-eval baselines that mention them (e.g. 'auth service') should be re-checked against the hermetic corpus.

**Value of simplifying.** Deletes a dead taxonomy and offset machinery (~150-200 lines), removes a dev fixture from every tenant's production retrieval planning, and makes the planner contract honest: strings in, seed strings out.

**Adversarial verifier: 🟡 ADJUSTED.** Core claim CONFIRMED on every factual point. (1) crates/moa-brain/src/planning/ner.rs is 398 lines producing NerSpan{start,end,text,label} with a 6-variant NerLabel; grep of the whole workspace shows NerLabel::Person/Org/Place are constructed nowhere (only Concept, Other, Product appear, all inside ner.rs), and no code outside ner.rs reads .label/.start/.end — the hits in pipeline/memory/* and retrieval/* are hit.node.label (NodeLabel, unrelated type). (2) planner.rs:161-164 maps spans to span.text only; planner.rs:354 infer_label_hint(text, _spans) ignores spans and keys off raw query text. (3) crates/moa-brain/assets/ner-gazetteer.txt is exactly 12 MOA-dev entries (auth service, postgres, restate, fly.io, aws, pgvector, ...) and the production retriever bakes it in via QueryPlanner::new() at crates/moa-brain/src/pipeline/memory.rs:209. (4) No load-bearing constraint forces the shape: extraction is a pure deterministic function (no Restate/replay concern), NerSpan has no serde derives (no wire format), docs mention NER only as 'planner NER seeds' (docs/eval/memory-eval-pipeline.md:89) — i.e. seed strings — and AGENTS.md's no-shims rule favors the clean break. The proposed Vec<String> return works: the only production consumer wants strings. BUT the claimed side effects need three corrections, hence 'adjusted': (a) crates/moa-brain/tests/brain_db_memory/hybrid_retrieval_db_memory.rs:115 does NOT use with_gazetteer as claimed — it uses QueryPlanner::new(); it survives anyway because that test only asserts reranker defaults and never calls plan(). (b) Missed consumer: crates/moa-eval/src/memory_eval/runner/mod.rs:558 constructs QueryPlanner::new() for every hermetic memory-eval probe, and the eval corpus vocabulary overlaps the gazetteer ('auth service' etc.), so emptying the production default silently changes the pinned memory-eval baseline unless the runner is switched to with_gazetteer/with_ner with the moved gazetteer file — the claimant's own '(or move ner-gazetteer.txt into the eval)' option must be treated as required, not optional, and gated on an eval baseline run per this repo's established practice of gating planner trims on eval sweeps. (c) The offsets are not purely dead: dedupe_spans (ner.rs:318-336) sorts by start (longest-text tiebreak) before applying the MAX_SPANS=12 cap, so for span-rich queries the offsets decide WHICH candidates survive; deleting them requires a deterministic first-seen dedup and can shift seed sets for long queries — deterministic and fine, but a behavior change beyond 'the 12 dev terms', so eval re-baseline covers more than the gazetteer removal. Also planning/mod.rs:6 pub-re-exports NerExtractor/NerLabel/NerSpan and must be trimmed.

> **Revised simpler alternative:** As proposed — extractor returns Vec<String>; delete NerSpan, NerLabel, offset bookkeeping, and the _spans parameter of infer_label_hint; update planning/mod.rs:6 re-exports — with two amendments: (1) keep dedup deterministic (first-seen order across the fixed extractor sequence, lowercase-keyed, capped at 12) since the cap decides which candidates become seeds; (2) do not merely 'optionally' move ner-gazetteer.txt — actually wire it into crates/moa-eval/src/memory_eval/runner/mod.rs:558 (currently QueryPlanner::new()) via with_gazetteer, or accept a memory-eval re-baseline, before emptying the production default.

> **Revised side effects:** Corrected: hybrid_retrieval_db_memory.rs:115 uses QueryPlanner::new(), not with_gazetteer, but is unaffected (it never calls plan(); it only asserts QueryRetrievalCtx reranker defaults). Missed: (1) crates/moa-eval/src/memory_eval/runner/mod.rs:558 uses QueryPlanner::new() for every hermetic memory-eval probe, so removing the default gazetteer changes pinned baselines for probes mentioning gazetteer terms — switch the runner to the moved gazetteer or refresh baselines, gated on an eval sweep (this repo has precedent for 'safe-looking' planner trims regressing eval coverage). (2) Dropping start/end changes span selection under the MAX_SPANS=12 cap for long queries (dedupe_spans currently orders by position with longest-text preference), so seed sets can shift beyond queries containing the 12 dev terms. (3) planning/mod.rs:6 re-exports NerExtractor/NerLabel/NerSpan and needs trimming; golden_eval.rs:432 (with_ner+with_gazetteer) keeps compiling if with_gazetteer keeps its signature.

---

### 27. Retrieval cache carries a dead per-tenant version-TTL cache and a config struct never customized anywhere

**Area:** moa-brain (context pipeline)
effort: **small** · finder confidence: **high** · ~LOC removable: **~100**

**Locations**

- `crates/moa-brain/src/retrieval/cache.rs:18-27`
- `crates/moa-brain/src/retrieval/cache.rs:113-139`
- `crates/moa-brain/src/retrieval/cache.rs:163-263`
- `crates/moa-brain/src/pipeline/memory.rs:174-181`

**What it is.** CachedHybridRetriever (the read-time retrieval cache, itself justified by the perf-gate baseline) contains a SECOND moka cache `versions` gated by `version_ttl`. DEFAULT_VERSION_TTL is Duration::ZERO, and current_version_cached short-circuits past the cache whenever the TTL is zero — so the versions cache is never consulted. Grepping the workspace: `version_ttl` is only ever set to ZERO (two tests), and `max_tenants`, `tenant_capacity`, and `cache_user_scope` are never set anywhere; there is no TOML/env wiring for CachedHybridRetrieverConfig at all. Production construction (pipeline/memory.rs:174-181) always uses `new`/`new_for_app_role` with the defaults, and the `with_config`/`with_config_for_app_role` constructors are test-only.

**Why it may be over-engineered.** An entire caching layer (second moka cache, TTL knob, cached-vs-direct branch) exists solely for a hypothetical tuning scenario the codebase cannot even reach: no config path can set version_ttl to non-zero, so the code that would serve stale-version hits is dead. The 5-field config struct is speculative surface — every field is default-only in production.

**Simpler alternative.** Delete the `versions` cache, `version_ttl`, and current_version_cached (call version_reader.current_version directly, which is today's actual behavior); delete `cache_user_scope` (the bypass-user-scope rule becomes a plain constant policy); collapse the constructor set to `new(inner, pool)` and `new_for_app_role(inner, pool)` plus a test-only capacity/ttl override if the two db_memory tests need it. If per-tenant version caching is ever wanted, reintroduce it behind a real MoaConfig knob with a measurement.

**Side effects / what to watch.** hybrid_retrieval_db_memory.rs and tenant_contact_knowledge_retrieval_db_memory.rs construct with explicit config (tenant_capacity/ttl) and need the retained test override or direct field defaults; no behavior change in production since the deleted branch is unreachable.

**Value of simplifying.** Removes a dead moving part (a second cache with its own expiry semantics) and a five-knob config surface that suggests tunability that does not exist, ~80-120 lines.

**Adversarial verifier: 🟡 ADJUSTED.** Core claim verified against crates/moa-brain/src/retrieval/cache.rs and crates/moa-brain/src/pipeline/memory.rs. DEFAULT_VERSION_TTL is Duration::ZERO (cache.rs:26) and current_version_cached short-circuits past the `versions` moka cache whenever ttl.is_zero() (cache.rs:251-252), so the version cache is unreachable in production. The only production construction site (pipeline/memory.rs:173-183) uses new/new_for_app_role with CachedHybridRetrieverConfig::default(); the factory has &MoaConfig in scope but never maps any of it to the cache config, and a workspace-wide grep (code, TOML, docs, compose, scripts) found zero external wiring for max_tenants/tenant_capacity/version_ttl/cache_user_scope. git log -S shows the entire version_ttl mechanism was added in commit 0652a05d (2026-07-03) with the default already ZERO — it has never been enabled. No load-bearing constraint refutes deletion: docs/18-performance.md, docs/02-brain-orchestration.md, docs/08-security.md, and docs/operations/tenant-vector-promotion-runbook.md never mention version_ttl; the runbook's reliance on changelog_version invalidation is preserved because the ZERO path is behaviorally identical to calling version_reader.current_version directly. Hardcoding the contact-scope cache bypass (cacheable_scope, cache.rs:366-368) as a constant policy is security-neutral-or-better. However the claim has two factual errors: (1) version_ttl is NOT only ever set to ZERO — unit test version_check_is_cached_within_ttl (cache.rs:571-602) sets it to 60s and is the sole exerciser of the dead branch, and tenant_capacity IS set (to 1) in unit test cache_eviction_at_capacity_forces_backend_re_miss (cache.rs:633-644) via the private with_parts constructor; (2) the two db_memory tests (crates/moa-brain/tests/brain_db_memory/hybrid_retrieval_db_memory.rs:107-114, tenant_contact_knowledge_retrieval_db_memory.rs:863-870) set ONLY version_ttl: Duration::ZERO — the default — so they need no retained override at all, making the simplification even cleaner than proposed. The db_memory test comments ("Disable the version-read TTL so invalidation assertions observe changelog bumps immediately") show the author anticipated a future non-zero default, but that is speculative intent with no reachable code path or measurement, which the repo's pre-prod no-speculative-surface posture does not protect.

> **Revised simpler alternative:** As claimed, delete the `versions` cache, `version_ttl`, and `current_version_cached` (call version_reader.current_version directly) and hardcode the contact-scope bypass in cacheable_scope, but go further than the claim: the two db_memory tests set only version_ttl: Duration::ZERO (the default), so no public test-only config override needs to be retained — collapse the public constructors to new(inner, pool) and new_for_app_role(inner, pool) and switch both db_memory harnesses to new_for_app_role. Keep tenant_capacity/ttl reachable only through the already-private with_parts (or plain parameters) for the in-crate unit tests; the pub CachedHybridRetrieverConfig export in crates/moa-brain/src/retrieval/mod.rs:9 can then be removed since its only external users are those two db_memory tests.

> **Revised side effects:** (1) Unit test version_check_is_cached_within_ttl (cache.rs:571-602) pins the deleted version-TTL behavior and must be deleted, not adapted — the claimant missed this. (2) Unit test cache_eviction_at_capacity_forces_backend_re_miss (cache.rs:633-644) needs tenant_capacity=1 via the private with_parts path, so a private capacity knob must survive. (3) The two db_memory tests need only a constructor swap to new_for_app_role plus removal of their now-stale "Disable the version-read TTL" comments — they do NOT set tenant_capacity as the claim stated. (4) Remove CachedHybridRetrieverConfig from the pub re-export at crates/moa-brain/src/retrieval/mod.rs:9 (and both db_memory imports). (5) No production behavior change: the ZERO path already reads the version directly per retrieval, and the "stale"/"hit" metric semantics in retrieve/retrieve_cached are untouched.

---

### 28. Near-duplicate build_semantic / build_vector embedding builder paths

**Area:** moa-providers
effort: **small** · finder confidence: **high** · ~LOC removable: **~80**

**Locations**

- `crates/moa-providers/src/embedding/factory.rs:74-135`
- `crates/moa-providers/src/embedding/factory.rs:137-210`

**What it is.** EmbeddingProviderKind has two ~70-line builder methods, build_semantic (for memory.embedding_model) and build_vector (for memory.vector.embedder.name). Both match the same four providers (OpenAI, Cohere, Gemini, ZeroEntropy), read the same API keys, and apply identical embed_pacer_override/concurrency_override wiring per arm. The only real differences: build_vector pins/validates output_dim from config and passes an EmbedderConstructionRole to Gemini.

**Why it may be over-engineered.** Eight nearly identical match arms exist where four would do. The pacer/concurrency override wiring is copy-pasted 7 times across the two methods (plus once in build_gemini_embedder). A change to override handling must be edited in up to 8 places.

**Simpler alternative.** Collapse into one fn build(config, model, dims: Option<usize>, role: EmbedderConstructionRole) with a small helper that applies the pacer/concurrency overrides to any provider (a closure or a 3-line generic fn over the builder pattern). build_semantic == build(.., dims: None, role: Retrieval); build_vector == build(.., dims: Some(cfg.output_dim), role).

**Side effects / what to watch.** Must preserve the semantic path's provider-default dimensions vs the vector path's pinned/validated dimensions; the existing factory unit tests pin both behaviors, so regressions surface immediately.

**Value of simplifying.** Roughly halves the factory's builder code and leaves one place to wire per-provider rate/concurrency overrides.

**Adversarial verifier: 🟡 ADJUSTED.** Factual check (crates/moa-providers/src/embedding/factory.rs): the duplication is real and the claim's line ranges are accurate. build_semantic (L74-135) and build_vector (L137-210) each match the same four EmbeddingProviderKind variants, and the embed_pacer_override/concurrency_override wiring block is copy-pasted 7 times across them plus an 8th copy in build_gemini_embedder (L240-258). Both methods are private; the only entry points are the public fns build_embedding_provider_from_config (semantic) and build_embedder_from_config (vector), whose external callers (crates/moa-orchestrator/src/runtime/deps.rs:112,295; crates/moa-orchestrator/src/services/knowledge/ingest.rs:156; crates/moa-orchestrator/src/services/memory/retrieval.rs:207; crates/moa-brain/src/pipeline/builder.rs:116) would be untouched by the merge. No docs/scripts reference the private methods. The semantic-vs-vector API-key asymmetry is cosmetic: CohereEmbedding::from_config_with_model (cohere.rs:57) and ZeroEntropyEmbedding::from_config_with_model (zeroentropy.rs:54) are literally required_config_secret + Self::new — identical to what build_vector inlines. The module's load-bearing invariant (header comment L3-14: no cross-provider failover for embeddings, compile-level) is about not wiring fallback chains and is fully preserved by a single build(config, model, dims: Option<usize>, role) fn. No Restate-determinism, security, or test-lane constraint applies — this is startup-time construction code, and the existing unit tests (factory.rs L384-427: semantic Cohere pins 1536 default dims, vector Cohere pins 1024 configured dims; L430 pins missing-key soft-disable which lives in the caller, not the builders) pin both behaviors. However two details in the claim need correction, hence adjusted rather than confirmed. (1) The claimed side effect 'semantic path uses provider-default dimensions' is wrong for Gemini: build_gemini_embedder unconditionally reads config.memory.vector.embedder.output_dim and .gemini.default_role even on the semantic path (factory.rs L244-250), so semantic Gemini is already pinned to the vector config's output_dim — an implementer of the merged fn must not 'fix' Gemini to honor dims=None, or behavior changes. (2) The '3-line generic fn' for override application is optimistic: OpenAIEmbedding, CohereEmbedding, ZeroEntropyEmbedding, and GeminiEmbeddingEmbedder share no trait exposing with_rate_limits/with_max_concurrent_requests, so the helper needs a small private trait implemented for the four types (or a macro_rules!), ~10-15 lines — still a clear net win (8 copies to 1) and each arm must still pass its own provider config section (openai/cohere/zeroentropy/google) for the override values.

> **Revised simpler alternative:** Merge into one private fn build(self, config: &MoaConfig, model: String, dims: Option<usize>, role: EmbedderConstructionRole) -> Result<Arc<dyn EmbeddingProvider>> with four arms. Per arm: OpenAI validates provider.dimensions() == d only when dims=Some(d); Cohere/ZeroEntropy apply .with_dimensions(d)? only when Some(d) (their from_config_with_model helpers can be deleted since they are just read_api_key + new); Gemini ignores dims entirely and keeps delegating to build_gemini_embedder(config, role) — it already pins cfg.output_dim from memory.vector.embedder on both paths, and that must stay. Callers: build_embedding_provider_from_config uses build(.., None, EmbedderConstructionRole::Retrieval); build_embedder_from_config uses build(.., Some(config.memory.vector.embedder.output_dim), role). For the override wiring, add a small private trait (or macro) with with_rate_limits/with_max_concurrent_requests implemented for the four embedder types plus one apply_overrides(provider, max_inputs_per_min, max_concurrent_requests) helper, collapsing the 8 copies (including the one in build_gemini_embedder) to 1.

> **Revised side effects:** Preserve: (a) semantic OpenAI/Cohere/ZeroEntropy use provider-default dimensions while the vector path pins/validates output_dim — pinned by factory unit tests (semantic Cohere 1536 vs vector Cohere 1024); (b) semantic Gemini does NOT get provider-default dims — it already reads memory.vector.embedder.output_dim and gemini.default_role via build_gemini_embedder, so a merged fn treating dims=None as 'provider default' for Gemini would silently change the served embedding dimensionality; (c) the missing-API-key soft-disable (MissingEnvironmentVariable -> Ok(None) + warn) lives in build_embedding_provider_from_config, not the builders, so the merge must keep returning that error variant untouched; (d) the two Gemini model-mismatch error strings differ ('gemini embedding provider' vs 'gemini vector embedder') and will collapse to one — harmless pre-prod, no compat requirement. Public API surface and all four external call sites are unchanged.

---

### 29. Backwards-compat catalog entry gemini-3.1-flash-lite-preview is fully redundant

**Area:** moa-providers
effort: **small** · finder confidence: **high** · ~LOC removable: **~50**

**Locations**

- `crates/moa-providers/src/core/models.rs:486-527`
- `crates/moa-providers/src/core/models.rs:660-676`

**What it is.** The model catalog contains both gemini-3.1-flash-lite (GA) and gemini-3.1-flash-lite-preview with byte-identical metadata; the comment says the legacy preview id is 'retained so existing configs keep routing', and a dedicated test pins that both ids resolve to their own entries.

**Why it may be over-engineered.** MOA is pre-prod with no external users, so config compat is explicitly a non-goal. Moreover the entry is redundant even for compat: find_model/find_for_provider_model use longest-prefix matching, so 'gemini-3.1-flash-lite-preview' would resolve to the GA entry (identical capabilities/pricing) with the entry deleted, and canonical_model_id returns the input id unchanged either way — zero behavior change.

**Simpler alternative.** Delete the preview ProviderModel entry, the flash_lite_ga_and_preview_ids_route_to_their_own_entries test, and the preview assertions in the gemini adapter tests; keep one GA entry.

**Side effects / what to watch.** None observable: prefix matching keeps the preview id routable with identical metadata. Only risk is if the preview id's pricing ever diverges from GA, which would then need a new entry.

**Value of simplifying.** Deletes a compat-only catalog entry and the tests that exist solely to pin it, per the repo's no-backwards-compat policy.

**Adversarial verifier: 🟡 ADJUSTED.** Factual accuracy: verified. crates/moa-providers/src/core/models.rs:488-527 contains two Google entries, "gemini-3.1-flash-lite" (L490) and "gemini-3.1-flash-lite-preview" (L510), with byte-identical context_window/max_output/capabilities/pricing (0.25/1.5/0.025) and tier Light; only display_name differs ("... (preview)"). The comment at L486-487 says the legacy id is "retained so existing configs keep routing". The pinning test flash_lite_ga_and_preview_ids_route_to_their_own_entries is at L661-676 as claimed. Prefix-matching claim verified: find_model (L545-550) and find_for_provider_model (L536-542) accept `model_id.starts_with(model.id)` and take the longest prefix, so with the preview entry deleted, "gemini-3.1-flash-lite-preview" resolves to the GA entry with identical capabilities and pricing; canonical_model_id (L576-584) returns the input string unchanged either way, so the routable id and billing are byte-for-byte unchanged. Hidden consumers: workspace-wide grep for "flash-lite-preview" and "flash-lite" (rs/toml/md/yaml/json/sh/env) hits only models.rs and crates/moa-providers/src/adapters/gemini/tests.rs — no config file, compose file, script, or doc references the preview id. ProviderModel.display_name has zero readers anywhere in the workspace (all .display_name reads are identity/agent/group types), so the lost "(preview)" label is unobservable. cheapest_chat_model (used by moa-orchestrator/src/services/narration.rs:158) picks gpt-5-nano per its own test (models.rs:796), so deletion cannot shift the narration default; even on a tie, GA precedes preview and min_by returns the first minimum. Load-bearing constraints: none — the catalog is pure static code, not Restate-journaled state; MOA is pre-prod and AGENTS.md/memory explicitly reject compat shims, so the "existing configs keep routing" rationale is a non-goal, and prefix matching preserves routing anyway. The one defect in the proposal: it misses that google_catalog_includes_latest_gemini_series (models.rs:657) asserts find("gemini-3.1-flash-lite-preview").is_some() using exact-match find() (L531-533), which WOULD fail after deletion; that assertion (and the L655 comment) must also be removed. Conversely, the gemini adapter test assertions the claim wants deleted (tests.rs:174-176 canonical passthrough and tests.rs:207-214 price envelope) would actually still pass after deletion via prefix matching — removing them is optional, and keeping tests.rs:174-176 would even pin that the legacy id still routes.

> **Revised simpler alternative:** Delete the gemini-3.1-flash-lite-preview ProviderModel entry (models.rs:508-527) and drop the "legacy preview id is retained" clause from the L486-487 comment; delete the flash_lite_ga_and_preview_ids_route_to_their_own_entries test (models.rs:660-676); ALSO remove the preview assertion and its comment from google_catalog_includes_latest_gemini_series (models.rs:655-657) — exact-match find() makes that assertion fail otherwise. The gemini adapter assertions (tests.rs:174-176, 207-214) still pass via prefix matching; delete them or keep tests.rs:174-176 as a cheap pin that the legacy id still routes through the GA entry.

> **Revised side effects:** As claimed, no observable production behavior change: the preview id still resolves via longest-prefix matching to identical capabilities, pricing, and unchanged canonical id. Two additions: (1) the display_name for the preview id silently becomes "Gemini 3.1 Flash-Lite" — unobservable today since no code reads ProviderModel.display_name, but worth knowing; (2) if the proposal is applied as originally written, google_catalog_includes_latest_gemini_series fails because find() is exact-match — fixed by the revised proposal. The pricing-divergence risk the claimant noted stands: if Google ever prices the preview id differently, a new entry would be needed.

---

### 30. Five external vendor adapters pre-production: Merge provider and Unstructured/Reducto parsers are parallel paths to the one live stack (Nango + native + LlamaParse)

**Area:** moa-knowledge
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~2000**

**Locations**

- `crates/moa-knowledge/src/providers/merge.rs`
- `crates/moa-knowledge/src/parser/unstructured.rs`
- `crates/moa-knowledge/src/parser/reducto.rs`
- `crates/moa-core/src/config/knowledge.rs:157-177,221-286`
- `crates/moa-core/src/config/env_overlay.rs:227-268`
- `crates/moa-knowledge/tests/knowledge_offline/provider_merge.rs`
- `crates/moa-knowledge/tests/knowledge_offline/parser_unstructured.rs`
- `crates/moa-knowledge/tests/knowledge_offline/parser_reducto.rs`

**What it is.** moa-knowledge ships two LinkedIntegrationProvider adapters (Nango, Merge) and three external DocumentParser adapters (LlamaParse, Unstructured, Reducto), each with its own HTTP adapter, config struct, env-overlay wiring, webhook verification branch, offline tests, and live tests. Defaults enable all of them (providers.enabled = [nango, merge]; parsers.enabled includes all four), but external_default is llamaparse and every API key defaults empty.

**Why it may be over-engineered.** MOA is pre-production with no users; only Nango Drive sync has been live-verified. Merge is half-wired: list_changed_records hardcodes the /api/knowledgebase/v1/articles endpoint regardless of the connector category, the advertised 'filestorage' category has no fetch_record_content implementation (the code comment says Merge 'would implement this same hook'), so filestorage records degrade to title-only ingestion. Unstructured and Reducto duplicate what the default LlamaParse adapter does, differing only in HTTP shape. Three near-duplicate parsers and a second half-working link provider are speculative parallel code paths kept 'just in case'.

**Simpler alternative.** Keep Nango + native + LlamaParse. Delete merge.rs, unstructured.rs, reducto.rs, their config structs (MergeKnowledgeProviderConfig, UnstructuredKnowledgeParserConfig, ReductoKnowledgeParserConfig), env overlays, the merge branches in edge/orchestrator webhook code, and their offline/live tests. The LinkedIntegrationProvider and DocumentParser traits stay, so re-adding a vendor later is one adapter file.

**Side effects / what to watch.** docs/21 and docs/04 describe Merge and the three parsers as intended surface, so this is a product-scope decision and docs need updating; the /v1/knowledge/webhooks/merge route and the 'merge' entries in selected_provider_api_key go away; if a customer later needs Merge unified categories or a different PDF parser, the adapter must be rebuilt (~300-400 lines each).

**Value of simplifying.** Deletes ~2000 lines (3 adapters ~1050, tests ~900, config/env ~150), removes three external vendor dependencies from the failure/behavior surface, and shrinks the config file and webhook matrix to what is actually exercised.

**Adversarial verifier: 🟡 ADJUSTED.** Every factual assertion in the claim checks out against the code. (1) Merge is genuinely half-wired: crates/moa-knowledge/src/providers/merge.rs:216 hardcodes `/api/knowledgebase/v1/articles` in list_changed_records regardless of connector category, while merge.rs:91-94 advertises both `knowledgebase` and `filestorage` categories via MERGE_KNOWLEDGE_CATEGORIES; MergeProvider does not override fetch_record_content, so it inherits the `Ok(None)` default at crates/moa-knowledge/src/providers/mod.rs:71-76, whose doc comment (mod.rs:63-64) literally says 'Merge filestorage would implement this same hook' — future tense, i.e. unimplemented. The ingestion pipeline (crates/moa-knowledge/src/ingestion.rs:1310-1365) then falls back to title-only input for such records. So a filestorage link degrades exactly as claimed, and a filestorage sync would list knowledgebase articles from the wrong endpoint. (2) The three external parsers fill the same DocumentParser slot: crates/moa-orchestrator/src/services/knowledge/ingest.rs:200-243 constructs LlamaParse/Unstructured/Reducto from parallel config blocks; defaults in crates/moa-core/src/config/knowledge.rs:87-133 enable all of them plus merge with external_default=llamaparse and every api_key empty; only LlamaParse can be reached without config changes. Live tests for all five adapters exist but are `#[ignore]`-gated (tests/parser_live.rs, tests/provider_live.rs) and per project memory only Nango Drive sync has ever been live-verified. (3) No load-bearing constraint forces the duplication: these are plain reqwest adapters behind traits — no Restate determinism, performance, or PII/security requirement depends on having a second link provider or a third parser (webhook signature verification is per-vendor but llamaparse alone keeps the parser-webhook path exercised; nango keeps the provider-webhook path). The traits stay under the proposal, so re-adding a vendor later is one adapter file plus config. (4) `merge` mentions in moa-orchestrator/src/workflows/consolidate.rs, services/skills.rs, and moa-loadtest/src/merge.rs are unrelated (memory dedup / report merging), so no hidden consumer there. The claim is therefore substantively CORRECT, but its side-effect list is incomplete and partly wrong, so the verdict is adjusted rather than confirmed: docs/04's 'merge' hits are memory consolidation, not the Merge vendor — the docs that actually name Merge/Unstructured/Reducto as intended surface are docs/21-tenant-knowledge-base.md (categories, webhook routes, test fakes), docs/01-architecture-overview.md:167 (the trait map that AGENTS.md declares the interface source of truth lists 'Nango and Merge adapters' as the LinkedIntegrationProvider implementations), and docs/10-technology-stack.md:98. The claimant also missed several concrete consumers that must be updated: .env.example lines 59-65 (MOA_MERGE_API_KEY, MOA_MERGE_WEBHOOK_SIGNATURE_KEY, MOA_UNSTRUCTURED_API_KEY, MOA_REDUCTO_API_KEY, MOA_REDUCTO_WEBHOOK_SIGNING_KEY); env_overlay.rs field declarations and the MOA_* mapping table around lines 554-557 plus overlay round-trip tests near line 1050; moa-edge KnowledgeWebhookConfig fields (routes.rs:84-93) and their wiring in moa-edge/src/main.rs:131-142, plus the merge/reducto arms in routes/webhook_verification.rs:25-44 and the two extra POST routes (routes.rs:149-160 registers /v1/knowledge/webhooks/reducto and /merge); orchestrator production wiring at services/knowledge/mod.rs:791-821 and 849-852; the merge-specific completion-signal match arms in services/knowledge/webhook.rs:311,358 and the "unstructured" entry in the parser-webhook match at webhook.rs:364 (already dead — no edge route exists for unstructured webhooks); orchestrator tests knowledge_service.rs:192-213 (pins Merge linked_account.synced as a completion signal), :300-385 (uses a fake 'merge' provider for list_integrations merging/filtering — must be renamed, not deleted, since it pins multi-provider merging), :469-494, :567; the reducto parser-webhook tests at knowledge_service.rs:680-695 (must be re-pinned on llamaparse to keep custom-header verification covered); and the fixture provider string 'merge' in moa-knowledge/tests/knowledge_db_memory/sync_run_db_memory.rs:33-36 (needs renaming if 'merge' leaves the default providers.enabled list at knowledge.rs:90).

> **Revised simpler alternative:** The claimant's proposal is workable as stated (delete merge.rs, unstructured.rs, reducto.rs, their config structs, env overlays, webhook branches, and tests; keep Nango + native + LlamaParse and the two traits). Additions for completeness: also remove the merge/reducto fields from moa-edge KnowledgeWebhookConfig and main.rs wiring, the /v1/knowledge/webhooks/reducto route, the 'unstructured' entry in the orchestrator parser-webhook match, the five .env.example lines, and the env_overlay field/mapping/test entries; shrink providers.enabled default to ["nango"] and parsers.enabled default to ["native", "llamaparse"]; rename the 'merge' fixture provider strings in knowledge_service.rs and sync_run_db_memory.rs rather than deleting those tests (they pin provider-merging and sync-run behavior, not Merge itself); re-pin the custom-header parser-webhook verification test on llamaparse; update docs/21, docs/01:167, and docs/10:98 (not docs/04).

> **Revised side effects:** Beyond the claimant's list (docs updates, /v1/knowledge/webhooks/merge route, selected_provider_api_key 'merge' entry, ~300-400 lines per adapter to rebuild later): (1) docs correction — docs/04 is NOT affected (its 'merge' is memory consolidation); the docs to update are docs/21-tenant-knowledge-base.md, docs/01-architecture-overview.md:167 (trait-map source of truth currently lists 'Nango and Merge adapters'), and docs/10-technology-stack.md:98. (2) .env.example lines 59-65 lose five vendor env vars; env_overlay.rs loses ~15 fields, their MOA_* mappings, and overlay round-trip test entries. (3) The /v1/knowledge/webhooks/reducto route also goes away (claimant only named the merge route), plus KnowledgeWebhookConfig fields merge_signature_key/reducto_signing_key/reducto_custom_header in moa-edge routes.rs:84-93 and main.rs:131-142, and the merge/reducto arms in webhook_verification.rs. (4) Orchestrator webhook.rs loses the merge completion-signal arms (lines 311, 358) and the already-dead 'unstructured' parser-webhook match entry (line 364). (5) Orchestrator integration tests need rework, not just deletion: knowledge_service.rs list_integrations tests use 'merge' as the second fake provider and pin multi-provider merging/filtering — rename the fake provider id; the Merge linked_account.synced completion-signal test is deleted with its production branch; the reducto custom-header webhook-verification test should be re-pinned on llamaparse so ParserWebhookVerifier custom-header coverage survives. (6) sync_run_db_memory.rs fixture connections use provider 'merge' and need renaming if 'merge' leaves the default providers.enabled list. (7) With only Nango implementing fetch_record_content, LinkedProviderContentFetcher and the RecordContentFetcher seam keep exactly one production implementer — fine to keep, but the mod.rs:63 doc comment referencing Merge filestorage must be rewritten.

---

### 31. Daytona and E2B cloud sandbox adapters are dead code behind feature flags no consumer enables

**Area:** moa-hands (tools/sandboxes/MCP)
effort: **medium** · finder confidence: **high** · ~LOC removable: **~2700 (delete) or ~60 of cfg/error plumbing (un-gate)**

**Locations**

- `crates/moa-hands/Cargo.toml:8-10`
- `crates/moa-hands/src/adapters/daytona/mod.rs`
- `crates/moa-hands/src/adapters/e2b/mod.rs`
- `crates/moa-hands/src/adapters/e2b/client.rs`
- `crates/moa-hands/src/adapters/e2b/tests.rs`
- `crates/moa-hands/src/core/construction.rs:88-120`
- `crates/moa-hands/src/core/construction.rs:308-347`
- `crates/moa-hands/tests/daytona_live.rs`
- `crates/moa-hands/tests/e2b_live.rs`
- `crates/moa-core/src/config/sandbox.rs:49-66`
- `crates/moa-core/src/config/env_overlay.rs:283-291`

**What it is.** Two full HandProvider implementations (Daytona ~714 LOC, E2B ~986 LOC incl. client and inline tests) plus ~850 LOC of live tests, cfg-gated registration in ToolRouter::from_config, a default_cloud_provider() function with per-feature cfg branches and 'feature disabled' error paths, the shared http_util module gating, and a CloudHandsConfig config section with env-overlay knobs (cloud_hands_daytona_api_key, cloud_hands_e2b_api_key, cloud_hands_default_provider).

**Why it may be over-engineered.** The `daytona` and `e2b` features are enabled by nothing in the workspace: no dependent crate's Cargo.toml, no Makefile target, no nextest profile, no script passes --features. The only production binary that builds a ToolRouter (moa-orchestrator via from_config) therefore can never register these providers, and all CloudHandsConfig fields and env overlays are knobs that cannot take effect. docs/06 calls Daytona the 'default cloud workspace provider', but as wired the deployed system supports only the local provider. Feature flags whose only effect is to keep ~1,900 LOC permanently compiled out are pure complexity; the cfg dance in construction.rs (including two unreachable 'feature is disabled' MoaError::Unsupported branches) exists solely to service flags nobody sets.

**Simpler alternative.** Pick one: (a) if cloud sandboxes are near-term, delete the two feature flags and compile the adapters unconditionally (they add no new dependencies; reqwest/eventsource-stream are already unconditional for MCP), removing all cfg attributes, the feature-disabled error branches in default_cloud_provider(), and the #[cfg(any(feature...))] gating on http_util and unsupported_tool(); registration is already config-driven at runtime. Or (b) if cloud sandboxes are not near-term, delete both adapter modules, the live tests, the CloudHandsConfig fields and env overlays, and the daytona/e2b arms of default_cloud_provider() — pre-production means they can be re-added cleanly when actually deployed.

**Side effects / what to watch.** Option (a): slightly longer compile for moa-hands dependents; live tests become runnable without a hand-typed --features flag (still env-gated). Option (b): loses ~2,700 LOC of working-but-unwired Daytona/E2B integration and its live test coverage; docs/06 provider map must be updated; HandHandle::Daytona/E2B variants in moa-core and their match arms (hand_id, tier labels) could also be pruned or left as-is.

**Value of simplifying.** Removes two permanently-dead modules (~2,700 LOC on the delete path) or removes the illusion that cloud sandboxes work when the deployed binary cannot enable them; either way eliminates config knobs that are never effective and cfg branches that mislead readers about the runtime provider set.

**Adversarial verifier: 🟡 ADJUSTED.** CORE CLAIM VERIFIED. (1) Feature flags gate zero dependencies: crates/moa-hands/Cargo.toml declares `daytona = []` and `e2b = []` (empty dep lists); reqwest/eventsource-stream are unconditional deps. (2) No consumer enables them: moa-orchestrator/Cargo.toml's [features] section (auth0, provider-overrides, skill-learning, experiments, slack, postmark, twilio, redis, integration, internal-eval-runner) has no daytona/e2b forwarding, and all four moa-hands dependents (moa-orchestrator, moa-brain, moa-eval, moa-memory-ingest) use `moa-hands = { workspace = true }` with no features. Grep of Makefile, scripts/*.sh (run-clean-e2e.sh ORCH_FEATURES="provider-overrides,skill-learning,redis"), .github/workflows (deploy.yml builds with only --features auth0), and .config/nextest.toml found zero `--features daytona/e2b`. The only production ToolRouter construction is moa-orchestrator/src/runtime/deps.rs:114 via from_config, so the cfg-gated registration blocks in construction.rs:88-120 never compile in any built binary, and CloudHandsConfig daytona/e2b fields plus env_overlay.rs:283-291 knobs are inert. (3) Live tests are dead even as documented: tests/daytona_live.rs and e2b_live.rs start with `#![cfg(feature = "daytona"/"e2b")]` so they compile to empty binaries without a hand-typed --features flag, and the certify skill's test matrix (.agents/skills/certify/references/test-matrix.md:111-112) references nonexistent targets `--test daytona_provider` / `--test e2b_provider` — stale wiring. The e2b inline offline tests (adapters/e2b/tests.rs, 192 LOC mock-TCP tests) also never compile in any CI lane. (4) No load-bearing constraint: AGENTS.md rule 6 ties feature flags to *optional dependencies* — inapplicable since these gate none; provider-integration SKILL.md line 70's gating rule is explicitly justified as "Don't pull in the dependency on default builds" — also inapplicable. No Restate/perf/security doc forces this. TWO CORRECTIONS. First, the claim calls the two `MoaError::Unsupported` "feature is disabled" branches (construction.rs:322-328, 335-341) unreachable — factually wrong: with features off (the only build configuration that exists) those branches ARE compiled and fire at runtime whenever config sets default_provider to daytona/e2b. Worse, .env.example line 80 ships uncommented `MOA_CLOUD_HANDS_DEFAULT_PROVIDER=daytona`, so copying the template makes ToolRouter::from_config fail orchestrator startup. This is an active footgun that strengthens, not weakens, the case for removing the flags. Second, option (b) (delete the adapters) is the wrong pick and its side effects are understated: docs/00 defines MOA as cloud-first, docs/06:25 names Daytona the default cloud workspace provider, README markets Daytona/E2B, .env.example exposes the knobs, provider-integration skill references e2b/ as the canonical HTTP-sandbox example, and moa-core's HandHandle::Daytona/E2B variants plus lifecycle.rs:882-883 match arms are already unconditionally compiled. LOC is 2,548 total (1,700 gated source incl. inline tests + 848 live tests), not ~2,750.

> **Revised simpler alternative:** Adopt option (a) only: remove the `daytona` and `e2b` features from crates/moa-hands/Cargo.toml and delete every cfg attribute keyed on them — adapters/mod.rs (module + http_util gating), lib.rs re-exports, construction.rs:15-18/88-120 registration gates and both feature-disabled branches of default_cloud_provider() (which then always returns Some for daytona/e2b), tools/grep.rs (4 cfg sites), tools/file_outline.rs:29, tools/sandbox_descriptor.rs:169, and the `#![cfg(feature=...)]` headers of tests/daytona_live.rs and tests/e2b_live.rs. Registration stays config-driven at runtime (provider only registers when default_provider or a non-empty API key is set), and live tests stay #[ignore]-plus-MOA_RUN_LIVE_*_TESTS gated per AGENTS.md, so nothing bills by default. Do NOT take option (b): it contradicts docs/00 cloud-first direction, docs/06's provider table, README, and the provider-integration skill's canonical examples. Also fix the stale certify matrix entries (.agents/skills/certify/references/test-matrix.md:111-112 reference nonexistent daytona_provider/e2b_provider targets; the real files are daytona_live/e2b_live).

> **Revised side effects:** Beyond the claimant's list: (1) adapters/e2b/tests.rs (192 LOC of offline mock-TCP tests) starts compiling and running in the default deterministic lane for the first time — a coverage gain, but these tests have never run in CI and could surface latent flakes (they bind ephemeral 127.0.0.1 ports, so parallel-safety looks fine). (2) The .env.example footgun changes meaning: MOA_CLOUD_HANDS_DEFAULT_PROVIDER=daytona would no longer fail startup with Unsupported; instead DaytonaHandProvider::from_config runs, which will error differently (or register a provider with an empty key) — .env.example line 80 should be commented out or set to local as part of the change. (3) tools/grep.rs, file_outline.rs, and sandbox_descriptor.rs lose cfg branches, slightly changing which code paths default builds exercise for remote-hand tool fallbacks. (4) certify's test-matrix.md and provider-integration's hand-provider-checklist.md line 51 mention the feature/live-test recipe and need a one-line doc touch. (5) Compile-time cost is marginal: ~1,700 extra LOC in moa-hands, no new dependencies.

---

### 32. Stdio MCP transport (~450 LOC incl. concurrent demux machinery and a background reader task) is unreachable from any production path

**Area:** moa-hands (tools/sandboxes/MCP)
effort: **small** · finder confidence: **high** · ~LOC removable: **~450**

**Locations**

- `crates/moa-hands/src/adapters/mcp/mod.rs:166-386`
- `crates/moa-hands/src/adapters/mcp/mod.rs:489-525`
- `crates/moa-hands/src/core/construction.rs:293-306`
- `crates/moa-hands/src/adapters/mcp/tests.rs`
- `crates/moa-hands/tests/fixtures/mock_mcp_stdio_server.py`
- `crates/moa-core/src/config/sandbox.rs:72,112-116`

**What it is.** MCPClient supports a StdioTransport: child-process spawn, LSP-style Content-Length framing (write_framed_message/read_framed_message), a spawned background reader task, a PendingMap of oneshot senders for concurrent request demultiplexing, and a PendingGuard RAII type with a three-tier Drop strategy (try_lock, spawn-on-runtime, spin-yield loop) for cancel safety. moa-core carries the McpTransportConfig::Stdio variant plus command/args/env config fields to support it.

**Why it may be over-engineered.** ToolRouter::from_config — the only production router construction path (moa-orchestrator runtime deps) — calls validate_mcp_transports_for_deployment first, which unconditionally errors on any stdio MCP server; there is no local-dev bypass. MCPClient::connect is otherwise only called from the private load_mcp_servers and reconnect_mcp_client (both post-validation) and from tests. So the entire stdio transport, its concurrency-safe demux machinery, and its python mock-server fixture exist only to be exercised by the crate's own tests of that same transport. docs/06 says stdio is 'allowed only for local development', but no code path actually allows it anywhere.

**Simpler alternative.** Delete StdioTransport, PendingMap/PendingGuard, run_reader, the framing helpers, the McpTransport enum (RemoteClient becomes the client directly), the stdio-specific arms of MCPClient::health_check/classify_error, the stdio tests and the mock_mcp_stdio_server.py fixture; then delete McpTransportConfig::Stdio and the command/args/env fields from moa-core config, which makes validate_mcp_transports_for_deployment and its two tests unnecessary (the config simply cannot express stdio). Pre-production: no config migration concerns.

**Side effects / what to watch.** Local development loses the (currently impossible anyway) option of stdio MCP servers; any future local-dev stdio support would need to be rebuilt or vendored via an MCP crate. The from_config_rejects_stdio test and mcp/tests.rs are removed with the feature. Removes one background task type from the system.

**Value of simplifying.** Deletes ~450 LOC including the most intricate concurrency code in the crate (RAII drop guard with runtime-handle fallback and a spin loop), a background task, a subprocess integration, and a config surface — all serving zero reachable functionality.

**Adversarial verifier: 🟡 ADJUSTED.** Core claim verified against the code. (1) crates/moa-hands/src/core/construction.rs:72-73: ToolRouter::from_config calls validate_mcp_transports_for_deployment first, which (L293-306) unconditionally errors on McpTransportConfig::Stdio — no deployment-mode flag or local-dev bypass exists (checked moa-core/src/config/sandbox.rs env overlays and MoaConfig). (2) Only production construction site is crates/moa-orchestrator/src/runtime/deps.rs:114 (from_config); ToolRouter::new_local (moa-eval/src/setup.rs:237, moa-brain tests) never loads MCP servers; reconnect_mcp_client (crates/moa-hands/src/core/dispatch.rs:435-446) reads self.mcp_servers populated only post-validation in load_mcp_servers (construction.rs:280); MCPClient is pub-exported (moa-hands/src/lib.rs:12) but workspace grep shows no external MCPClient::connect caller. So StdioTransport, PendingMap/PendingGuard (three-tier Drop at mod.rs:325-350), run_reader, and the framing helpers (mod.rs:489-525) are reachable only from the crate's own tests — dead in production, exactly as claimed. docs/06-hands-and-mcp.md:182-183 says stdio is 'allowed only for local development' but no code path allows it; the test comment in tests/hands_offline/mcp_router.rs:25-26 claiming local-dev stdio 'lives on new_local' is aspirational (new_local has no MCP loading). No Restate-determinism, performance, security, or test-lane constraint forces the complexity; docs/08 treats stdio as the weaker isolation boundary. However the claimant's proposal/side-effects have errors: mcp/tests.rs is mostly remote-transport coverage (only 1 of 4 tests is stdio — the HTTP header-injection, SSE-parsing, and flatten tests must stay); tests/hands_offline/mcp_router.rs keeps its credential-proxy and fail-closed tests; McpTransportConfig carries #[default] Stdio (sandbox.rs:73) so deleting the variant forces a new default or a required field; MCPClient::classify_error/reconnect machinery in recovery.rs must survive (remote ReProvision via gateway-failure escalation at recovery.rs:316-323 uses it); docs/06 and docs/implementation-caveats.md:55-59 need updating.

> **Revised simpler alternative:** Delete only the stdio-specific machinery: StdioTransport, PendingMap, PendingGuard, run_reader, write_framed_message/read_framed_message, the McpTransport enum (make RemoteClient the client's transport directly), the stdio arms of health_check (health_check then becomes always-true and can be removed along with its recovery preflight call) and the stdio message-matching in classify_error (which then reduces to moa_core::classify_tool_error — delete the wrapper and call classify_tool_error directly in recovery.rs, keeping the ReProvision/reconnect path intact since remote gateway failures still use it). In moa-core, delete McpTransportConfig::Stdio plus command/args/env fields, and either move #[default] to Http or make `transport` a required field so configs omitting it fail loudly instead of silently changing meaning. Delete validate_mcp_transports_for_deployment and its two unit tests, the from_config_rejects_stdio_mcp_transport_for_deployment test in tests/hands_offline/mcp_router.rs, the stdio_client_lists_and_calls_tools test only (keep the other three tests in src/adapters/mcp/tests.rs), and tests/fixtures/mock_mcp_stdio_server.py. Update docs/06-hands-and-mcp.md (supported transports, stdio paragraphs at L179-200) and docs/implementation-caveats.md (L55-59) to say only HTTP/SSE are supported.

> **Revised side effects:** Beyond the claimant's list: (a) src/adapters/mcp/tests.rs is NOT removed — 3 of its 4 tests pin remote HTTP/SSE behavior and stay; only the stdio test goes. (b) tests/hands_offline/mcp_router.rs keeps its credential-proxy and fail-closed tests. (c) The serde default for McpServerConfig.transport is currently Stdio; removing the variant changes what a transport-omitting TOML config means (was: rejected at startup; becomes: silently the new default) unless transport is made a required field. (d) MCPClient::health_check and the recovery health-check preflight become vacuous for remote-only transports and should be removed with the transport, slightly enlarging the diff. (e) MCPClient::classify_error collapses to classify_tool_error; recovery.rs call sites switch to the core function — the ReProvision/reconnect_mcp_client machinery must be kept because remote gateway failures still trigger it. (f) docs/06-hands-and-mcp.md and docs/implementation-caveats.md contain stdio prose that must be updated. (g) graphify graph should be refreshed after the change per AGENTS.md.

---

### 33. Three of five public ToolRouter execute entry points (plus eager install_files) have no production callers and duplicate ~50 lines of span/policy boilerplate each

**Area:** moa-hands (tools/sandboxes/MCP)
effort: **small** · finder confidence: **high** · ~LOC removable: **~170**

**Locations**

- `crates/moa-hands/src/core/dispatch.rs:25-78`
- `crates/moa-hands/src/core/dispatch.rs:81-88`
- `crates/moa-hands/src/core/dispatch.rs:130-182`
- `crates/moa-hands/src/core/lifecycle.rs:44-58`

**What it is.** ToolRouter exposes execute (inline policy check, no recovery), execute_authorized (thin wrapper), execute_authorized_with_cancel, execute_with_recovery (inline policy check + recovery), and execute_authorized_with_recovery. execute and execute_with_recovery each replicate the same ~50-line block: create tool_execution_span, prepare_invocation, registry lookup, record metadata, match on ActionPolicyEffect with identical Deny/AdminReview error formatting, record result. lifecycle.rs also exposes a public eager install_files that provisions a hand and installs files immediately.

**Why it may be over-engineered.** Production uses exactly two entry points: moa-brain harness calls execute_authorized_with_cancel and moa-orchestrator tool_executor calls execute_authorized_with_recovery (policy is evaluated separately via check_policy/prepare_invocation on the governed orchestrator path). execute, execute_authorized, and execute_with_recovery are called only by moa-hands' own tests, and ToolRouter::install_files is called only by two offline tests (production uses the lazy set_trusted_sandbox_files + install_trusted_files_for_hand path). These are near-duplicate parallel code paths kept alive by tests, and their inline policy-effect matching duplicates the Deny/AdminReview handling that the real production policy path implements differently (dispatch.rs returns PermissionDenied for AdminReview; the documented production behavior queues a review).

**Simpler alternative.** Delete execute, execute_with_recovery, and the eager install_files; keep execute_authorized (or inline it) as the test-facing wrapper over execute_authorized_with_cancel if tests want the short form; port the handful of tests that exercised inline policy (e.g. docker_hardening_docker, one recovery test) to call check_policy + execute_authorized_with_cancel explicitly, which is exactly what production does. Extract the shared span/metadata/result-recording wrapper into one private helper used by the two surviving entry points.

**Side effects / what to watch.** Test call sites in moa-hands (docker_hardening_docker.rs, local_tools_docker.rs, local_tools_offline.rs, mcp_router.rs, and the daytona/e2b live tests) need mechanical updates. No production behavior change. The divergent AdminReview semantics of the deleted paths disappear, removing a trap where a future caller could get an error instead of the documented review-queuing behavior.

**Value of simplifying.** ~170 LOC deleted, one public API instead of five near-clones, and eliminates a semantic trap (two different AdminReview behaviors on the same struct).

**Adversarial verifier: 🟡 ADJUSTED.** Factual core CONFIRMED by reading the code. (1) crates/moa-hands/src/core/dispatch.rs matches the claim exactly: execute (L25-78), execute_authorized (L81-88, a 3-line wrapper), execute_authorized_with_cancel (L91-127), execute_with_recovery (L130-182), execute_authorized_with_recovery (L189-223). execute and execute_with_recovery each replicate the same ~50-line span/prepare_invocation/registry-lookup/metadata/policy-match/record block, with byte-identical Deny and AdminReview arms differing only in the inner call (execute_authorized_inner vs execute_authorized_with_recovery_inner). (2) Production callers, verified by workspace-wide grep: exactly two — crates/moa-orchestrator/src/services/tool_executor.rs:137 calls execute_authorized_with_recovery (after set_trusted_sandbox_files at :133, policy handled separately via services/action_policy.rs:126 prepare_invocation and the action_reviews review-queue app), and crates/moa-brain/src/harness/tool_dispatch.rs:278 calls execute_authorized_with_cancel (after its own prepare_invocation + Deny/AdminReview branching at tool_dispatch.rs:115-230). (3) ToolRouter::execute is called only by two offline tests (crates/moa-hands/tests/hands_offline/local_tools_offline.rs:808, :901); execute_with_recovery only by one (:856); ToolRouter::install_files (lifecycle.rs:44-58) only by local_tools_offline.rs:741 and :783 — production uses set_trusted_sandbox_files + install_trusted_files_for_hand (tool_executor.rs:133, turn_execution.rs:876, moa-brain/src/harness/streaming/mod.rs:424). No hidden consumers in scripts/, docs/, compose, or other crates (docs/06-hands-and-mcp.md mentions only HandProvider::install_files, a different trait method; the provider.execute() calls in docker_hardening_docker.rs/local_tools_docker.rs/e2b_live.rs/daytona_live.rs are HandProvider::execute, not ToolRouter methods). (4) The AdminReview divergence is real: dispatch.rs's inline arm returns PermissionDenied, while the brain harness emits a ToolError event explaining it has no durable review queue and the orchestrator path routes through the action_reviews app (crates/moa-orchestrator/src/action_reviews/app.rs request_review/decide_review), tested in tests/integration/action_policy_flow_e2e.rs. (5) No load-bearing constraint found: Restate determinism concerns only the tool_executor path (execute_authorized_with_recovery survives); nothing in docs or AGENTS.md forces the parallel entry points; AGENTS.md rule 7 actually argues against keeping wrapper paths. ADJUSTMENTS: the claimed side-effect list is wrong in both directions — docker_hardening_docker.rs and local_tools_docker.rs call HandProvider::execute, not the deleted methods, so they need no changes; conversely the ~40 execute_authorized call sites (local_tools_offline.rs, mcp_router.rs, session_search_db.rs, daytona_live.rs, e2b_live.rs) need no changes either since the proposal keeps execute_authorized. Also the test-porting suggestion is flawed: a ported test that calls check_policy then conditionally calls execute_authorized_with_cancel would be tautological (it re-implements the caller's branching and asserts its own logic). The moa-brain harness Deny/AdminReview branches currently have no direct tests (grep of crates/moa-brain/tests shows no ActionPolicyEffect coverage), so the offline gate tests at local_tools_offline.rs:803/850/895 are today's only sub-e2e pins that Deny prevents tool-body execution on the local path; that pin must move to moa-brain's dispatch_tool_call, not be mechanically ported.

> **Revised simpler alternative:** Delete ToolRouter::execute, execute_with_recovery, and the eager install_files; keep execute_authorized as the thin test-facing wrapper (it already delegates to execute_authorized_with_cancel). Extract the shared span/prepare/metadata/result-recording wrapper into one private helper used by execute_authorized_with_cancel and execute_authorized_with_recovery. Do NOT port the deny/admin-review gate tests to a check_policy + execute_authorized_with_cancel sequence in moa-hands — that re-implements the caller's branching and pins nothing. Instead, move the enforcement pin to the real production gate: add moa-brain tests asserting dispatch_tool_call returns ToolCallOutcome::Skipped and emits the ToolError event for Deny and AdminReview effects without invoking the router execute path (the orchestrator side is already covered by tests/integration/action_policy_flow_e2e.rs). Port the two install_files tests to set_trusted_sandbox_files + a tool execution that triggers install_trusted_files_for_hand, which is the production lazy path.

> **Revised side effects:** Smaller blast radius than claimed, plus one test-coverage transfer obligation. Only five call sites need changes, all in crates/moa-hands/tests/hands_offline/local_tools_offline.rs: execute at :808 and :901, execute_with_recovery at :856, install_files at :741 and :783. No changes to docker_hardening_docker.rs or local_tools_docker.rs (they call HandProvider::execute directly), and none to the ~40 execute_authorized sites in local_tools_offline.rs, mcp_router.rs, session_search_db.rs, daytona_live.rs, or e2b_live.rs since execute_authorized is kept. Real risk the claimant missed: the deleted offline tests are currently the ONLY sub-e2e tests pinning that a Deny policy prevents the tool body from running on the local/brain path (moa-brain has no ActionPolicyEffect tests; orchestrator coverage lives in action_policy_flow_e2e.rs). Deleting them without adding equivalent moa-brain coverage silently drops that enforcement pin.

---

### 34. recovery.rs duplicates the entire retry/reprovision state machine for hand vs MCP execution, including a 12-line counter-update block copy-pasted four times

**Area:** moa-hands (tools/sandboxes/MCP)
effort: **medium** · finder confidence: **high** · ~LOC removable: **~280**

**Locations**

- `crates/moa-hands/src/core/recovery.rs:90-268`
- `crates/moa-hands/src/core/recovery.rs:270-415`
- `crates/moa-hands/src/core/recovery.rs:417-473`
- `crates/moa-hands/src/core/recovery.rs:475-526`

**What it is.** execute_hand_with_recovery (~180 LOC) and execute_mcp_with_recovery (~145 LOC) are structurally identical loops: health_check, classify error, gateway-escalation upgrade (Retryable -> ReProvision after consecutive 502-504s), handle_failure, then update retry_attempts/reprovisions/consecutive_timeouts/consecutive_gateway_failures. That counter-update block appears verbatim four times (pre-execution and post-execution error arms in each loop). handle_hand_failure and handle_mcp_failure are also near-identical, differing only in the hand_id vs None result tag and reprovision_hand vs reconnect_mcp_client recovery action, each with its own single-use FailureContext struct.

**Why it may be over-engineered.** The retry/idempotency semantics themselves are mandated by docs/06, but nothing forces two parallel implementations: the loops differ in exactly three pluggable points (how to get/validate the backend, how to execute once, how to reprovision). Every future fix to the escalation logic (e.g. the gateway-unavailable upgrade rule) must be applied in four places, and the two failure handlers have already started to drift only in cosmetic ways.

**Simpler alternative.** Collapse into one generic recovery loop: a small RecoveryTarget abstraction (three async closures or a private trait: health_check(), execute_once(), reprovision(), plus the result-tag), a single FailureContext, and a single counters struct with an update(class, error) method replacing the four copy-pasted blocks. execute_hand_with_recovery and execute_mcp_with_recovery become ~15-line adapters.

**Side effects / what to watch.** Refactor of the crate's most behavior-sensitive file; the existing recovery/tests.rs suite (411 LOC) pins the semantics and should pass unchanged, which also serves as the mutation check. No API change.

**Value of simplifying.** ~250-300 LOC removed and, more importantly, the retry-escalation policy becomes single-source so a future tweak cannot silently apply to hand tools but not MCP tools (or vice versa).

**Adversarial verifier: 🟡 ADJUSTED.** Core claim verified against crates/moa-hands/src/core/recovery.rs (621 LOC). The 12-line counter-update block appears verbatim four times (lines 180-196, 248-264, 341-357, 395-411) plus two 3-line variants in the health-check Ok(false) arms; the gateway-escalation upgrade (Retryable + HttpStatus 502..=504 + consecutive_gateway_failures>=1 -> ReProvision) is copy-pasted four times (151-158, 219-226, 316-323, 370-377). handle_hand_failure (417-473) and handle_mcp_failure (475-526) differ only in label, log string, result tag Some(hand_id) vs None, and reprovision_hand vs reconnect_mcp_client. No hidden consumers: all symbols are private to the file; the sole caller is execute_authorized_with_recovery_inner in the same file, and workspace-wide grep found no references in other crates, tests, scripts, configs, or docs. No load-bearing constraint: docs/06-hands-and-mcp.md mandates the Retryable/ReProvision/Fatal semantics and idempotency classes but not dual implementations; backoff is plain tokio::time::sleep inside one activity so Restate determinism is untouched; a monomorphized private trait has zero runtime cost. Two inaccuracies force adjustment: (1) the abstraction needs more than "three async closures" — the loops differ in acquire-backend-per-iteration, health_check, error classification (asymmetric: HandProvider::classify_error is async and needs the live HandHandle, MCPClient::classify_error is static/sync), execute_once, reprovision, result-tag, and provider label; a private trait with an associated Backend type covers it but the shape is a 4-6-method trait, not three closures. (2) The claimed mutation check is wrong: recovery/tests.rs (411 LOC) has zero MCP coverage — every test drives the hand path via a mock HandProvider — so "suite passes unchanged" pins only the hand half of the refactor; the MCP adapter (reconnect wiring, None tag) is currently unpinned.

> **Revised simpler alternative:** Collapse into one generic recovery loop driven by a small private trait (not bare closures) with an associated Backend type: acquire() -> Backend (called at the top of each loop iteration, since HandHandle is re-provisioned per iteration), health_check(&Backend), classify(&Backend, &MoaError, consecutive_timeouts) -> ToolFailureClass (async to accommodate HandProvider::classify_error; the MCP impl ignores the backend and wraps the static MCPClient::classify_error), execute_once(&Backend), reprovision(), result_tag(&Backend) -> Option<String>, and label() -> &str for metrics/log text. One FailureContext, one counters struct with an update(class_was_retryable, error) method replacing the four verbatim blocks, and one handle_failure. execute_hand_with_recovery / execute_mcp_with_recovery become thin adapters constructing the trait impls. Keep the loop's Cancelled early-returns and the BeforeExecution vs AfterUncertainExecution stage distinction exactly as-is.

> **Revised side effects:** The existing recovery/tests.rs suite (411 LOC) pins only the hand path — it has no MCP tests at all — so passing unchanged does NOT mutation-check the MCP adapter half of the refactor (wrong reprovision action or result tag on the MCP side would go undetected). Either add one MCP-path recovery test alongside the refactor or manually verify the MCP adapter wiring (reconnect_mcp_client, None tag, server_name label). Upside the claimant missed: after unification the hand tests pin the shared loop for both backends, closing the current MCP coverage gap. No API change confirmed: all refactored symbols are file-private with a single same-file caller.

---

### 35. ToolRouter carries a concrete LocalHandProvider side-channel next to the dyn HandProvider map, special-cased by provider-name string comparison

**Area:** moa-hands (tools/sandboxes/MCP)
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~50 net (moves more than it deletes)**

**Locations**

- `crates/moa-hands/src/core/mod.rs:39-40`
- `crates/moa-hands/src/core/dispatch.rs:362-368`
- `crates/moa-hands/src/core/lifecycle.rs:734-758`
- `crates/moa-hands/src/adapters/local/mod.rs:365-471`

**What it is.** ToolRouter stores both providers: HashMap<String, Arc<dyn HandProvider>> and local_provider: Option<Arc<LocalHandProvider>> pointing at the same object. Three call sites branch on `provider == DEFAULT_PROVIDER_NAME` and use the concrete handle to reach methods missing from the trait: execute_with_cancel (hard-cancel support), lease_handle (durable reconnect metadata), and adopt_lease_handle (cache rehydration). Non-local providers get generic fallbacks (tokio::select cancellation wrapper; LeaseHandle::new without metadata).

**Why it may be over-engineered.** This is a parallel representation of one object plus stringly-typed downcasting. In the production build only the local provider exists anyway (see finding 1), so the dyn map currently holds exactly one entry and every dispatch first consults the map and then string-compares its own key to decide whether to bypass it. The trait abstraction the docs mandate is undermined by the side-channel: adding cancellation or reconnect-metadata support to Daytona/E2B would require more special cases rather than a trait method.

**Simpler alternative.** Extend the HandProvider trait (moa-core) with default-implemented methods: execute_with_cancel(handle, tool, input, token) whose default wraps execute in tokio::select (exactly the current generic fallback), and reconnect_metadata(handle) -> Result<Option<serde_json::Value>> / adopt_reconnect_metadata(handle, Option<&Value>) whose defaults return None/Ok (the current non-local behavior). LocalHandProvider overrides them; delete the local_provider field, lease_handle_for_provider, hydrate_lease_handle's special case, and all DEFAULT_PROVIDER_NAME comparisons in dispatch/lifecycle. LeaseHandle keeps its JSON metadata payload unchanged.

**Side effects / what to watch.** Touches the moa-core HandProvider trait (allowed: pre-production, and default impls keep other implementors source-compatible). Slightly widens the trait surface; the LeaseHandle struct stays in moa-hands since only JSON crosses the trait boundary. Recovery and lease tests should pass unchanged.

**Value of simplifying.** Removes a duplicate field, three stringly-typed dispatch branches, and makes cancellation/reconnect behavior uniform across providers — future cloud providers get correct hard-cancel handling for free instead of the select-based approximation.

**Adversarial verifier: 🟡 ADJUSTED.** Factual accuracy: verified. crates/moa-hands/src/core/mod.rs:39-40 stores both `providers: HashMap<String, Arc<dyn HandProvider>>` and `local_provider: Option<Arc<LocalHandProvider>>`; construction.rs:53-66 and :76-130 insert the same Arc into both. The three cited string-compare bypass sites exist exactly as claimed: dispatch.rs:362-368 (execute_with_cancel with generic tokio::select fallback at :369-378), lifecycle.rs:734-745 (lease_handle_for_provider, generic fallback LeaseHandle::new without metadata), lifecycle.rs:747-758 (hydrate_lease_handle). The concrete methods live at adapters/local/mod.rs:365-522. `local_provider` has no other consumers anywhere in the workspace (grep over crates/): only construction + the three sites.

"Only the local provider exists in production": true for every current build. moa-hands Cargo.toml has `default = []`, `daytona = []`, `e2b = []`, and no crate in the workspace (moa-brain, moa-eval, moa-orchestrator, moa-memory-ingest Cargo.tomls) enables either feature. from_config CAN insert Daytona/E2B providers when the features are on (construction.rs:88-120), so the dyn map is not dead code, but today it always holds exactly one "local" entry, and dispatch consults the map then string-compares its own key, as claimed.

Feasibility of the alternative: workable. The HandProvider trait (moa-core/src/traits/mod.rs:583-629) is #[async_trait] and already has default-implemented methods (install_files :594, classify_error :601, health_check :611), so default methods are an established pattern there. moa-core already depends on tokio-util and traits/mod.rs already imports CancellationToken (:15) and uses it in ToolContext (:729), so execute_with_cancel needs no new dependency. LeaseHandle (core/leases.rs:18-24) is just HandHandle + Option<serde_json::Value>, and only the JSON crosses the boundary, so LeaseHandle stays in moa-hands as claimed. Notably the alternative can be simpler than proposed: LocalHandProvider::adopt_lease_handle (adapters/local/mod.rs:407-471) always returns lease_handle.handle.clone() unchanged — it only rehydrates in-process caches — so the trait method can be `adopt_reconnect_metadata(&self, handle: &HandHandle, metadata: Option<&Value>) -> Result<()>` with the router returning the lease's handle itself. Daytona/E2B would inherit exactly today's generic behavior via the defaults, and adding real cancel/reconnect support to them becomes a trait override instead of another string special case.

Load-bearing constraints: none force the side-channel. Lease persistence format (LeaseHandle JSON in the hand-lease store) is unchanged; Restate replay determinism (docs/02) is untouched — this is tool-execution-side, not orchestration state; no perf (docs/18) or security (docs/08) doc pins this shape. Tests: recovery/tests.rs and lifecycle.rs tests register mocks under `provider.provider_name()` (their own names, not "local") and rely on the generic fallbacks, which the trait defaults reproduce byte-for-byte; local_tools_docker.rs:147 calls execute_with_cancel on LocalHandProvider directly, which survives as the override. As a bonus, routers built via ToolRouter::new (which sets local_provider: None, construction.rs:31) currently hit a latent runtime error ("local provider missing from tool router") if anything dispatches to "local"; the trait approach removes that trap.

Corrections to the claim (why adjusted, not plain confirmed): (1) the proposal says it deletes "all DEFAULT_PROVIDER_NAME comparisons in dispatch/lifecycle" — lifecycle.rs:650 (tenant workspace_mount only for local+SandboxTier::Local) is a fourth comparison the proposal does not address and which remains; registration.rs:60/70 and construction.rs:314-316 uses also stay. (2) Side effects missed: AGENTS.md declares trait definitions in docs/01-architecture-overview.md the interface source of truth — the HandProvider row (docs/01:160, "Provision, execute, pause/resume, destroy hands") and docs/06-hands-and-mcp.md:29-32 must be updated when the trait widens.

> **Revised simpler alternative:** As proposed, with two refinements: (1) the adopt-side trait method can be `async fn adopt_reconnect_metadata(&self, handle: &HandHandle, metadata: Option<&serde_json::Value>) -> Result<()>` (default Ok(())) because LocalHandProvider::adopt_lease_handle never alters the handle — hydrate_lease_handle then always returns lease_handle.handle.clone() after the call, removing the need for a handle-returning signature. (2) Scope note: only the three cited special cases are removed; the lifecycle.rs:650 workspace_mount local-tier branch and the DEFAULT_PROVIDER_NAME uses in registration.rs/construction.rs are separate concerns and remain. moa-core needs no new dependency: tokio_util::sync::CancellationToken is already imported and used in traits/mod.rs (ToolContext).

> **Revised side effects:** In addition to the claimed side effects: (a) docs/01-architecture-overview.md trait table (HandProvider row, line ~160) and docs/06-hands-and-mcp.md must be updated — AGENTS.md makes docs/01 the trait source of truth; (b) minor behavior change: a router built via ToolRouter::new with a provider registered under the name "local" (currently impossible to use — dispatch errors "local provider missing from tool router") would start working via the default trait paths; no existing test registers a mock as "local", so nothing breaks; (c) Daytona/E2B adapters (feature-gated, currently enabled by no crate) compile against the widened trait via the defaults with zero code changes and identical runtime behavior; recovery/lease tests pass unchanged as claimed since mocks exercise exactly the code the defaults reproduce.

---

### 36. Seven single-implementation session-store facet traits plus a SessionRepository aggregate, all backed by one PostgresSessionStore

**Area:** session / db / migrations / runtime-store
effort: **medium** · finder confidence: **high** · ~LOC removable: **~650**

**Locations**

- `crates/moa-core/src/traits/mod.rs:209-471`
- `crates/moa-session/src/store/session_store.rs:743-808`
- `crates/moa-session/src/store/session_store.rs:1471-1619`
- `crates/moa-session/src/store/mod.rs:539-607`
- `crates/moa-session/src/store/learning.rs:113-130`
- `crates/moa-orchestrator/src/ctx.rs:26-134`
- `crates/moa-eval/src/setup.rs:120-135`

**What it is.** moa-core defines SegmentStore, ExperienceStore, LearningCandidateStore, SessionAnalyticsStore, SessionEventLookupStore, SessionLearningLogStore, and SessionChannelStore, plus a SessionRepository supertrait with a blanket impl bundling all of them. PostgresSessionStore implements each via ~330 lines of impl blocks that delegate 1:1 to inherent methods with identical signatures. Orchestrator PersistenceDeps then stores 10 separate Arc<dyn ...> fields that are all clones of the same Arc<PostgresSessionStore>, each with its own accessor; moa-eval setup does the same fan-out.

**Why it may be over-engineered.** Every one of the seven facet traits has exactly one implementation (grep 'impl X for' confirms) and zero test doubles — only SessionStore itself and ActionPolicyRuleStore have mocks and are justified. Consumers bypass the abstraction anyway: 8 of the 10 PersistenceDeps accessors (.segment_store(), .experience_store(), .analytics_store(), .event_lookup_store(), .learning_log_store(), .channel_store(), .action_policy_store(), .session_event_store()) have ZERO call sites outside ctx.rs; .learning_candidate_store() has one; production code reaches for .session_store_backend() (the concrete PostgresSessionStore) instead, and moa-edge analytics routes call the moa_session::analytics free functions directly rather than the SessionAnalyticsStore trait. All consuming crates (orchestrator, brain, eval, edge) already depend on moa-session in Cargo.toml, so the traits do not even break a crate dependency. docs/01's trait map does not list these traits, so no architecture-doc mandate applies.

**Simpler alternative.** Delete the seven facet traits and SessionRepository from moa-core, delete the delegating impl blocks in moa-session (keep the inherent methods), and replace the 10 trait-typed Arc fields in PersistenceDeps (and moa-eval SetupStores) with the single Arc<PostgresSessionStore> that already exists as session_store_backend. Keep SessionStore (6 real mocks in moa-brain/moa-hands/moa-eval tests) and ActionPolicyRuleStore (StaticRuleStore mock). moa-brain's SkillInjector::with_segment_store takes Arc<PostgresSessionStore> directly, or keep SegmentStore alone if a mock is planned.

**Side effects / what to watch.** Mechanical churn across moa-core, moa-session, moa-orchestrator ctx/deps, moa-brain builder, moa-eval setup; loses the ability to mock the seven facets in future tests (none exist today); services/experiments.rs one call site retargets to the concrete store.

**Value of simplifying.** Deletes ~650 lines (260 trait definitions, ~330 delegation, ~90 ctx fields/accessors), removes an indirection layer nobody uses, and makes 'where does this query live' a one-hop answer instead of trait -> impl -> inherent method.

**Adversarial verifier: 🟡 ADJUSTED.** Structure confirmed but the claim's central premise — "consumers bypass the abstraction anyway" — is factually wrong, and the deletion proposal contradicts a documented, CI-enforced architecture policy. Evidence: (1) OrchestratorCtx::current_session_store() returns Arc<dyn SessionRepository> and has ~30 production call sites (crates/moa-orchestrator/src/workflows/turn_execution.rs x5, services/contacts.rs x8, services/tool_executor.rs x4, services/action_reviews.rs x2, workflows/consolidate.rs, workflows/progress_delivery.rs, objects/worker/handlers.rs, objects/session/handlers.rs, workflows/procedure_node_actions.rs, experiment workflows) that exercise six of the seven facets THROUGH the trait: append_learning (SessionLearningLogStore), create_segment/get_active_segment/complete_segment (SegmentStore), get/replace_session_channel_binding (SessionChannelStore), tool_event_exists/action_review_event_exists (SessionEventLookupStore), append_experience_record (ExperienceStore), append_learning_candidate (LearningCandidateStore). session_store_backend() has only 7 call sites, each documented as an exception. (2) docs/15-architecture-policy.md "Modular Monolith Boundary Policy" mandates extraction readiness: "A future deployed split must be replaceable from composition code without changing turn workflows, handler contracts, or domain tests" — the SessionRepository seam is that mechanism; replacing it with Arc<PostgresSessionStore> at 30+ workflow call sites violates the policy. (3) crates/xtask/src/check_architecture_boundaries.rs enforces this direction with counted allowances: the moa-core LOC budget reason (line 786) explicitly budgets "narrow session repository traits", and ~10 RuntimeContext allowance reasons name "the session-store seam" as the sanctioned path (e.g., line 395 frames concrete-backend use as an exception "for transaction-aware skill promotion"). (4) Deleting the facets by merging methods into SessionStore would bloat 6 real SessionStore mocks (moa-hands/src/tools/tool_result.rs:524, four moa-brain test-support mocks, moa-eval/src/long_conversation/memory_metrics.rs:216) with ~40 stub methods; the facet split is what keeps SessionStore mockable. (5) moa-brain SkillInjector consumes Arc<dyn SegmentStore> in production (pipeline/skills/activation.rs calls list_skill_resolution_rates/list_task_strategy_success_rates); taking Arc<PostgresSessionStore> would couple a domain crate to a Postgres repository, contra docs/15's domain-crate row. (6) moa-eval SetupStores facet fields are NOT dead: transcript_runner.rs:826-894 uses segment_store/experience_store/learning_candidate_store. What the claim got right: PersistenceDeps' 10-field fan-out is redundant — 7 accessors (segment_store, experience_store, analytics_store, event_lookup_store, learning_log_store, channel_store, session_event_store) have zero call sites, action_policy_store() is only self-referential, and composition in runtime/deps.rs already coerces the concrete Arc directly; and SessionAnalyticsStore has zero trait-mediated consumers (moa-edge routes/analytics.rs uses moa_session::analytics free functions on Arc<PostgresSessionStore>, routes.rs:74).

> **Revised simpler alternative:** Keep the seven facet traits, the SessionRepository aggregate, and the delegating impls in moa-session — they are the CI-tracked extraction-readiness seam (docs/15, xtask check-architecture-boundaries) and are exercised through Arc<dyn SessionRepository> at ~30 call sites. Simplify only the redundant fan-out: collapse PersistenceDeps to session_repository (Arc<dyn SessionRepository>), session_store_backend (Arc<PostgresSessionStore>), and graph_pool; delete the 7 dead facet fields/accessors plus the unused action_policy_store and session_event_store accessors (callers needing a facet coerce from session_repository or the backend); retarget services/experiments.rs:269 from runtime.learning_candidate_store() to runtime.session_store() (SessionRepository already includes LearningCandidateStore); optionally collapse moa-eval SetupStores' facet-typed fields into one Arc<dyn SessionRepository> since transcript_runner uses several facets on the same object. Optionally also delete SessionAnalyticsStore from moa-core and drop it from the SessionRepository supertrait, since no consumer uses it through the trait (moa-edge uses moa_session::analytics free functions and the concrete store's inherent methods) — but leave the other six facets alone.

> **Revised side effects:** The trimmed change touches only crates/moa-orchestrator/src/ctx.rs, services/experiments.rs (one call site), optionally moa-eval/src/setup.rs + long_conversation/transcript_runner.rs, and (if SessionAnalyticsStore is dropped) crates/moa-core/src/traits/mod.rs, crates/moa-core/src/lib.rs re-exports, and crates/moa-session/src/store/mod.rs:539-607. It must keep xtask check-architecture-boundaries green: allowance needles like ".session_store()" and "OrchestratorCtx::current_session_store" have exact expected counts that fail on drift in either direction, and the moa-core pub-use symbol budget (max 87) and LOC budget reasons reference the session repository traits — removing SessionAnalyticsStore requires updating the LOC budget number/reason string. No mocks break (none exist for the facets). The original claim's proposal would additionally have violated docs/15's extraction-readiness policy, required rewriting ~10 xtask allowance entries whose reasons name the session-store seam, and coupled moa-brain (a domain crate) to PostgresSessionStore.

---

### 37. moa-migrations keeps three parallel schema sources: a 24-file incremental chain, hand-curated per-domain replay subsets, and a hand-copied SQL prefix guarded by a sync test — plus an ownership manifest no tool reads

**Area:** session / db / migrations / runtime-store
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~400 plus 24->4 SQL files**

**Locations**

- `crates/moa-migrations/src/lib.rs:18-181`
- `crates/moa-migrations/src/lib.rs:524-537`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/migrations/postgres/`
- `crates/xtask/src/main.rs:51-162`

**What it is.** Production runs the refinery chain of 24 incremental migrations. Schema-isolated tests replay hand-maintained const lists (SESSION_SCHEMA_MIGRATIONS with 16 entries, plus AUTH/ORCHESTRATOR/OCSF lists) that must be updated whenever a migration lands. Because V000302 mixes session-owned and artifact-owned DDL, a 70-line ACTION_POLICY_SCHEMA_MIGRATION_SQL const hand-copies its prefix and a unit test asserts it stays byte-identical. Separately, migration-ownership.toml (190 lines) declares per-table owners and claims 'Keep this in sync with cargo xtask check-migrations' — but check-migrations never parses that file; it only checks filename versions and duplicate CREATE TABLE ownership from the SQL itself.

**Why it may be over-engineered.** The repo's stated policy is that MOA is pre-production and migrations can be edited or renumbered in place with no compatibility shims. An incremental chain plus curated replay subsets plus a copy-sync test is compatibility machinery: every new migration is a chance to forget the subset entry or drift the copied prefix. The ownership manifest is dead data — nothing enforces it, so it can only rot.

**Simpler alternative.** Squash the 24 migrations into one baseline per domain (session/auth/orchestrator/ocsf), splitting the session-owned prefix of V000302 into the session baseline so ACTION_POLICY_SCHEMA_MIGRATION_SQL and its sync test disappear. The per-domain replay consts then each hold exactly one file and stop needing maintenance. Delete migration-ownership.toml or make check-migrations actually validate against it (deleting is simpler; the duplicate-CREATE-TABLE check already exists in xtask).

**Side effects / what to watch.** Long-running dev compose databases desync and need make dev-wipe (a known, accepted workflow per project notes); the test template rebuilds automatically via session_schema_fingerprint. Granular migration history is lost — explicitly acceptable pre-prod.

**Value of simplifying.** Removes a recurring drift hazard (subset lists, copied SQL, sync test), ~200 lines of Rust consts/tests, a 190-line dead manifest, and makes 'what schema do tests run' answerable by reading one file per domain.

**Adversarial verifier: 🟡 ADJUSTED.** Factual accuracy — verified true on every cited point. (1) crates/moa-migrations/migrations/postgres/ has exactly 24 files (V000001..V000321); production applies the refinery chain via moa_migrations::run (crates/moa-session/src/store/mod.rs:626-641 uses run_session_schema only for the schema-isolated test path, run() otherwise). (2) crates/moa-migrations/src/lib.rs:91-160 SESSION_SCHEMA_MIGRATIONS has 16 entries; AUTH (2), ORCHESTRATOR (1), OCSF (1) lists at lines 162-181. (3) ACTION_POLICY_SCHEMA_MIGRATION_SQL (lib.rs:20-89) is a hand-copied 70-line prefix of V000302, and the test at lib.rs:525-537 asserts byte-identity via split_once("\nALTER TABLE moa.artifact_run"). The suffix it strips is artifact/experiment constraint churn on moa.artifact_run/moa.experiment_* tables that are created in V000001 itself (V000001 lines 2357-2593), so the const exists only because V000302 mixes domains in one file — not a fundamental constraint. (4) migration-ownership.toml says "Keep this in sync with cargo run -p xtask -- check-migrations" but cmd_check_migrations (crates/xtask/src/main.rs:51-162) only checks filename versions, non-central migration dirs, and duplicate CREATE TABLE ownership derived from the SQL files themselves (migration_owner_key = file name, main.rs:164-166); a workspace-wide grep finds migration-ownership.toml referenced only by crates/moa-migrations/README.md — no tool parses it. Load-bearing constraints — none found: Restate replay determinism, perf, and PII docs are unrelated to migration file layout; the test-lane replay helpers and session_schema_fingerprint template rebuild (lib.rs:272-293, moa-session/src/testing.rs:256, 377) hash the consts' name+sql, so a squash auto-invalidates templates. CI runs check-migrations (.github/workflows/deploy.yml:95) but it is layout-agnostic. Strong extra evidence FOR the claim: the auth baseline V000101 already contains V000303's resolved_at column and idx_builtin_approvals_unresolved_terminal index (V000101 lines 117, 177-182) — the repo already edits baselines in place while keeping the now-redundant forward migration, which is exactly the duplication the claim attacks and proves the squash is consistent with existing practice. Why "adjusted" not "confirmed": the claimant missed hidden consumers and side effects. (a) crates/moa-experiments/tests/model.rs:127-266 include_str!s V000302 directly and text-parses its DROP/ADD CONSTRAINT blocks to pin ExperimentTrialStopReason values — squashing V000302's suffix into V000001's inline CHECK clauses breaks this test's parser, which must be rewritten against the baseline. (b) crates/moa-orchestrator/tests/orchestrator_db/eval_run_status_db.rs:13 include_str!s V000313 directly (a migration in NO replay list); it must be re-pointed after the fold. (c) V000312/V000320 (moa.hand_leases) and V000313 (analytics.eval_run_status) belong to no replay list today; folding them into a baseline requires a domain decision (hand_leases lives in the `moa` schema created by the session baseline, but is orchestrator/hands-owned per the toml — the orchestrator baseline would need its own CREATE SCHEMA IF NOT EXISTS moa, or they fold into the session baseline, enlarging the session test template — harmless, arguably better parity). (d) Side effects understate blast radius: docs/05-session-event-log.md says cloud deployments use managed Postgres/Neon, so any deployed database with recorded refinery checksums — not just dev compose — needs a wipe or manual refinery_schema_history repair after renumbering (refinery rejects checksum/version divergence); pre-prod policy accepts this but it should be stated. (e) crates/moa-migrations/README.md (lines 20-36) documents the ownership toml and a "forward migration after released checksum" policy and must be rewritten. On the toml: correct that deleting breaks nothing programmatic, but it carries human-only reader/ownership notes (e.g. the Auth0 users-provisioning note) that the SQL-derived xtask check cannot reproduce; deleting loses documentation, so stripping the false "in sync with check-migrations" sentence and keeping it as plain docs is an equally simple option.

> **Revised simpler alternative:** Squash per domain as proposed (session/auth/orchestrator/ocsf baselines; V000101 already absorbed V000303, proving the pattern), with three amendments: (1) assign the orphan migrations to a domain — fold V000313 (analytics.eval_run_status) and V000312+V000320 (moa.hand_leases) into either the session baseline (simplest: `moa`/`analytics` schemas are created there, and the test template gains production parity) or the orchestrator baseline with its own `CREATE SCHEMA IF NOT EXISTS moa`; (2) rewrite the two tests that include_str! specific migration files: crates/moa-experiments/tests/model.rs (parses V000302's DROP/ADD CONSTRAINT text — must parse the inline CHECK in the squashed baseline instead) and crates/moa-orchestrator/tests/orchestrator_db/eval_run_status_db.rs (includes V000313); (3) update crates/moa-migrations/README.md to drop the forward-migration/ownership-toml rules. For migration-ownership.toml, either delete it or just remove the false "keep in sync with check-migrations" claim and keep it as human documentation — both are net simplifications; do not build toml validation into xtask.

> **Revised side effects:** Beyond the claimed dev-compose desync: any deployed/managed Postgres (Neon, per docs/05-session-event-log.md) with recorded refinery checksums must be wiped or have refinery_schema_history manually repaired — refinery errors on missing/renumbered applied versions, it does not silently continue. Two tests pinned to specific migration files break and need rewriting (moa-experiments/tests/model.rs constraint-text parser on V000302; orchestrator eval_run_status_db.rs include of V000313). Session test templates grow to include hand_leases/eval_run_status tables previously absent (harmless; improves parity). crates/moa-migrations/README.md needs rewriting. session_schema_fingerprint changes force a one-time template DB rebuild (automatic, by design).

---

### 38. Duplicate plain vs control-plane analytics read paths where production only ever uses the control-plane variant

**Area:** session / db / migrations / runtime-store
effort: **small** · finder confidence: **high** · ~LOC removable: **~100**

**Locations**

- `crates/moa-session/src/analytics.rs:116-137`
- `crates/moa-session/src/analytics.rs:198-220`
- `crates/moa-session/src/store/mod.rs:279-326`
- `crates/moa-core/src/traits/mod.rs:265-292`
- `crates/moa-session/src/analytics.rs:487-555`

**What it is.** get_tenant_stats and list_cache_daily_metrics each exist twice: a plain pool-scoped variant and a _control_plane variant that wraps the same _with_conn helper in a ScopedConn::begin_control_plane transaction. Both variants are also mirrored as PostgresSessionStore methods and again as SessionAnalyticsStore trait methods. analytics.rs additionally hand-rolls session_status_from_db and parse_db_enum, duplicating queries::from_db which already does exactly this via strum FromStr.

**Why it may be over-engineered.** The only production caller (moa-edge routes/analytics.rs) uses exclusively the _control_plane variants; the plain variants are called only by moa-session's own db test and one brain test. Keeping both doubles the API surface (4 public functions, 4 store methods, 4 trait methods) for a distinction no caller exercises, and the duplicated enum parsers are a hand-rolled copy of an existing helper in the same crate.

**Simpler alternative.** Delete the plain get_tenant_stats/list_cache_daily_metrics functions and their store/trait mirrors, renaming the _control_plane variants to the plain names (the _with_conn helpers stay). Replace parse_db_enum and session_status_from_db with the existing queries::from_db. If finding 1 is applied, the trait mirror disappears entirely for free.

**Side effects / what to watch.** Two test call sites in moa-session/tests/postgres_store_db.rs and one in moa-brain switch to the control-plane-scoped call (behaviorally equivalent for reads, tests run as table owner).

**Value of simplifying.** Halves the analytics read API, removes a near-duplicate parallel code path kept 'just in case', and deletes redundant enum-parsing code.

**Adversarial verifier: 🟡 ADJUSTED.** Duplication verified at every cited location: crates/moa-session/src/analytics.rs:116-137 and 198-220 define plain + _control_plane pairs over shared _with_conn helpers; crates/moa-session/src/store/mod.rs:279-326 mirrors all four as PostgresSessionStore methods and 562-592 again in the trait impl; crates/moa-core/src/traits/mod.rs:265-292 carries all four as SessionAnalyticsStore trait methods. Workspace-wide grep (crates/, docs/, scripts/, migrations) shows the only production callers are crates/moa-edge/src/routes/analytics.rs:109, 218, 229, all using the _control_plane free functions; the plain variants are called only from crates/moa-session/tests/postgres_store_db.rs:1740 and 1751. The only SessionAnalyticsStore impl is PostgresSessionStore, and OrchestratorCtx::analytics_store() (crates/moa-orchestrator/src/ctx.rs:95) has zero call sites, so no dyn consumer or mock breaks. No load-bearing constraint: both queries read the daily_storage_partition_metrics MATERIALIZED VIEW (V000001__session_baseline.sql:605), Postgres RLS does not apply to matviews, and ScopedConn::begin_control_plane (crates/moa-db/src/lib.rs:58-69) only sets moa.* GUCs consumed by table policies (V000308) — so the plain vs control-plane split has no observable behavioral difference for these two reads at all today, and no doc mandates control-plane scoping for analytics reads. The enum-parser claim also holds: queries::from_db (crates/moa-session/src/queries/enums.rs:16) is pub(crate), same-crate, documented as "the single adapter", round-trip tested for SessionStatus and all three learning enums, and produces the byte-identical error string to session_status_from_db; parse_db_enum differs only in error wording ("invalid" vs "unknown"), which no caller matches on. The claim is adjusted, not confirmed, because one factual assertion is wrong: there is no moa-brain caller of these functions (grep of crates/moa-brain returns nothing), so the claimed side effect on a brain test does not exist — the simplification is strictly safer than claimed.

> **Revised simpler alternative:** As proposed: delete the plain get_tenant_stats/list_cache_daily_metrics free functions, rename the _control_plane variants to the plain names, keep the _with_conn helpers, and collapse the store/trait mirrors to one method each. Replace session_status_from_db with queries::from_db("session status", ...) (error strings are byte-identical) and replace parse_db_enum with from_db (only cosmetic error-text change from "invalid X `v`" to "unknown X value `v`"; from_db takes &str so pass &value at the three call sites in learning_candidate_summary_from_row).

> **Revised side effects:** Only two call sites are affected, both in crates/moa-session/tests/postgres_store_db.rs (lines 1740 and 1751), and after the rename they need no source change at all — the method names they call stay the same and now route through the control-plane-scoped transaction, which is behaviorally identical for these reads (the target is a materialized view exempt from RLS, and tests connect as the owning role anyway). There is no moa-brain caller, contrary to the original claim. No dyn SessionAnalyticsStore consumers or mocks exist beyond PostgresSessionStore, so the trait-method removal breaks nothing else.

**Implementation status: ✅ DONE.** Current code keeps only `get_tenant_stats` and `list_cache_daily_metrics`; both now open a control-plane scoped transaction internally. The `_control_plane` free functions, `PostgresSessionStore` methods, and `SessionAnalyticsStore` trait methods are gone, and `analytics.rs` uses `queries::from_db` for session and learning-candidate enum parsing. Verification passed with `cargo check -p moa-core -p moa-session -p moa-edge -p moa-orchestrator --all-targets --locked`; DB route/session tests were blocked by local Postgres maintenance-pool timeouts and recorded as REG-008 in `docs/simplification-deferred-regressions.md`.

---

### 39. moa-lineage-sink ships four dead or test-only parallel surfaces, including a backwards-compat decode shim

**Area:** lineage / ocsf / observability
effort: **small** · finder confidence: **high** · ~LOC removable: **~200**

**Locations**

- `crates/moa-lineage/sink/src/mpsc_sink.rs:59-104 (MpscSinkBuilder),307-333 (NullSink),354-358 (unused From<&WriterHandle> impl)`
- `crates/moa-lineage/sink/src/writer.rs:98-99 (LineageWriter marker),242-277 (WriterReceiver enum + pub spawn_writer),1194-1197 (decode_pending_row legacy fallback)`
- `crates/moa-lineage/sink/src/lib.rs:12-14`

**What it is.** The sink crate exports: (1) `MpscSinkBuilder`, a fluent builder over a 4-field config struct with zero consumers anywhere; (2) `NullSink`, whose own doc says production standardizes on `moa_core::NullLineageHandle` and that it is 'kept as a pub type for potential out-of-tree consumers'; (3) `LineageWriter`, an empty marker struct never referenced; (4) `pub spawn_writer` plus the `WriterReceiver::Raw` enum arm, a second writer entry point over a raw `mpsc::Receiver<LineageEvent>` used only by the crate's own writer_db.rs tests (production uses `spawn_writer_for_sink`); (5) `decode_pending_row`, which falls back to decoding journal payloads as a bare legacy `LineageRow` when the current `PendingRow` envelope fails; (6) an unused `From<&WriterHandle> for WriterStats` impl.

**Why it may be over-engineered.** MOA is pre-production with no out-of-tree consumers, so 'potential external users' and journal-format compatibility fallbacks are automatically over-engineering under the repo's own no-backwards-compat rule. The Raw/Commands receiver split doubles the writer's input plumbing purely so tests can avoid constructing `WriterCommand::Event` themselves.

**Simpler alternative.** Delete MpscSinkBuilder, NullSink, LineageWriter, and the From impl outright. Delete `spawn_writer` and the `WriterReceiver` enum; port writer_db.rs tests to send `WriterCommand::Event(Box::new(evt))` through `spawn_writer_for_sink` (or a pub(crate) test constructor). Reduce `decode_pending_row` to a single `serde_json::from_slice::<PendingRow>` — local journals can be wiped pre-prod.

**Side effects / what to watch.** writer_db.rs (4 call sites) and the null_sink unit test need mechanical updates; any developer machine with an old-format fjall journal must wipe it once.

**Value of simplifying.** Removes a duplicate writer entry path (one fewer place the batching/journal logic can diverge), a compat shim, and three dead public types from the crate's API surface.

**Adversarial verifier: 🟡 ADJUSTED.** Every factual element of the claim checks out against the real code, but the proposed test-port mechanism does not compile as stated. Verified facts: (1) MpscSinkBuilder (crates/moa-lineage/sink/src/mpsc_sink.rs:59-104, exported lib.rs:12) has zero consumers — both production call sites build the sink via MpscSink::spawn(MpscSinkConfig::from(&config.observability.lineage), pool) directly (crates/moa-orchestrator/src/lineage.rs:61-62, crates/moa-orchestrator/src/services/eval/mod.rs:1123-1124); only moa-orchestrator and moa-edge depend on the crate, and moa-edge only imports moa_lineage_sink::admin (crates/moa-edge/src/routes/lineage.rs:18). (2) NullSink is used only by its own unit test null_sink_never_records_drops; production uses moa_core::NullLineageHandle (lineage.rs:76), exactly as NullSink's own doc admits; its 'potential out-of-tree consumers' rationale is void pre-prod. (3) LineageWriter (writer.rs:99) is an empty marker struct with no references anywhere except the lib.rs:14 re-export. (4) From<&WriterHandle> for WriterStats (mpsc_sink.rs:354-358) has zero call sites; WriterStats is not referenced outside sink/src at all. (5) spawn_writer (writer.rs:269-277) and WriterReceiver::Raw (writer.rs:242-266) are consumed only by crates/moa-lineage/sink/tests/writer_db.rs (4 call sites), matching the claim. (6) decode_pending_row's bare-LineageRow fallback (writer.rs:1194-1197) exists solely for journals written before the PendingRow envelope landed in commit c8e18467 ('eval grafana alerts #161'); all current journal writers (append_event_row_sync writer.rs:136-145, append_event_rows writer.rs:152-177) emit PendingRow envelopes, and no test exercises the fallback. Load-bearing constraint check: the fjall journal is a local node-durability buffer, not Restate journal state, so docs/02 replay determinism does not apply; no security/PII or test-lane mandate forces any of these surfaces; the repo's explicit pre-prod no-backwards-compat rule directly condemns the decode shim and the 'out-of-tree consumers' NullSink. Where the claim fails: spawn_writer_for_sink, WriterCommand, and DurableJournal are all pub(crate) (writer.rs:235, 280, 103), and writer_db.rs is an integration test compiled as a separate crate — neither 'send WriterCommand::Event through spawn_writer_for_sink' nor 'a pub(crate) test constructor' is visible from tests/. The viable port (which I verified preserves all three tests' semantics) is the fully public production path: MpscSink::spawn(config, pool), enqueue via LineageSink::record(&sink, evt) (try_send into capacity-64 channel with 3 events cannot drop, so assertions stay deterministic), drop the sink to close the channel, then handle.shutdown(); the poison-batch recovery test still works because MpscSink::spawn reopens the same journal_path. The claimant also missed one side effect: crates/moa-lineage/README.md:22 lists NullSink in the crate's public surface and needs updating.

> **Revised simpler alternative:** Delete MpscSinkBuilder, NullSink (and its null_sink_never_records_drops unit test), LineageWriter, and the From<&WriterHandle> for WriterStats impl outright; trim the lib.rs re-exports accordingly. Delete pub spawn_writer and the entire WriterReceiver enum, changing spawn_writer_task to take mpsc::Receiver<WriterCommand> directly. Port the three writer_db.rs tests to the public production path instead of pub(crate) internals: let (sink, handle) = MpscSink::spawn(config, pool).await?, send events with LineageSink::record(&sink, evt), drop(sink) to close the channel, then handle.shutdown() — this exercises the real production entry point and keeps every existing assertion (shutdown drain count, dead-letter row, journal replay by a second MpscSink::spawn over the same journal_path). Reduce decode_pending_row to a bare serde_json::from_slice::<PendingRow>(payload) (or inline it at its single call site, writer.rs:408).

> **Revised side effects:** writer_db.rs's three tests (4 spawn_writer call sites) must be rewritten to MpscSink::spawn + LineageSink::record + drop(sink) — NOT to spawn_writer_for_sink/WriterCommand, which are pub(crate) and invisible to integration tests; the rewrite is behavior-preserving because the 64-capacity channel cannot drop 3 try_send events and MpscSink::spawn reopens the same journal path for the recovery phase. crates/moa-lineage/README.md:22 must drop NullSink from the listed public surface. Any developer journal written before commit c8e18467 would fail decode on replay (logged/counted, row retained unacked) — acceptable pre-prod with a one-time journal wipe, as claimed. No other consumers exist: only moa-orchestrator (MpscSink/MpscSinkConfig/WriterHandle, all kept) and moa-edge (admin module only) depend on the crate.

---

### 40. Three-tier artifact visibility machinery for a scope enum with exactly one variant

**Area:** skills / artifacts
effort: **medium** · finder confidence: **high** · ~LOC removable: **~250 in Rust + ~40 lines of schema**

**Locations**

- `crates/moa-artifacts/src/registry.rs:26-54 (ArtifactScopeParts)`
- `crates/moa-artifacts/src/registry.rs:576-643, 709-837, 1129-1339 (user_id binds/predicates in every query)`
- `crates/moa-skills/src/registry.rs:27-60, 377-411 (Skill.tenant_id/user_id/scope, skill_from_package_revision)`
- `crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql:2252-2420 (user_id + GENERATED scope columns on 5 artifact tables)`

**What it is.** The artifact registry is built for a global/tenant/user visibility hierarchy: ArtifactScopeParts carries Option<tenant_id>, Option<storage_partition_id>, Option<user_id>; every SQL statement binds user_id and filters on `a.user_id IS NULL` or `(a.user_id IS NULL OR a.user_id = $2)`; five Postgres tables (artifact, artifact_revision, artifact_file, artifact_run, artifact_node_run) carry nullable user_id plus a GENERATED `scope` tier column; StoredArtifactRevision/Skill expose user_id: Option<UserId> and scope: String fields; and moa-skills round-trips tenant_id by parsing it back out of the storage_partition_id string.

**Why it may be over-engineered.** moa_core::ActionRuleScope (crates/moa-core/src/types/action_policy.rs:120) has exactly one variant: Tenant. So user_id is always written NULL, scope is always 'tenant', and every Option is always Some/None in one fixed direction. docs/09-skills-and-learning.md confirms the design intent: skills are tenant-scope only, 'There is no contact-scoped skill inheritance.' Several queries (load_visible_with_status, list_visible, load_run, cancel_run, load_files) even bind a $2 user_id parameter the SQL never references — pure dead weight. The module doc still says 'MOA three-tier visibility'.

**Simpler alternative.** Collapse ArtifactScopeParts to a plain TenantId (or delete it and derive storage_partition_id inline from ActionRuleScope::Tenant). Delete all user_id binds and `user_id IS NULL` predicates, drop the user_id and generated scope columns from the five artifact tables (pre-prod: edit V000001 in place), make storage_partition_id NOT NULL, and remove Skill.user_id/Skill.scope and StoredArtifactRevision.user_id/scope. Store tenant_id directly on Skill instead of parsing it back from the partition string.

**Side effects / what to watch.** Requires editing the baseline migration and a dev-wipe of local DBs (per repo policy this is allowed and routine). Ripples into moa-orchestrator services/skills.rs helpers (skill_scope_from_stored_parts, memory_scope_from_skill) and a handful of _db_memory tests that assert scope strings. If a user-scoped artifact tier is genuinely planned, this deletes its runway — but re-adding a column later is cheaper than carrying dead tri-state logic through every query now.

**Value of simplifying.** Removes an always-NULL column and predicate from ~15 SQL statements across 5 tables, kills three Option fields that are never None/Some in practice, eliminates a class of silent bugs (dead binds already drifted out of sync with the SQL), and makes the actual authorization story — tenant RLS — legible.

**Adversarial verifier: 🟡 ADJUSTED.** FACTUAL CHECK — mostly accurate. ActionRuleScope has exactly one variant (crates/moa-core/src/types/action_policy.rs:120-127, `Tenant { tenant_id }`). ArtifactScopeParts::from_scope (crates/moa-artifacts/src/registry.rs:39-48) always yields Some(tenant)/Some(partition)/None(user). The dead-bind claim is CONFIRMED: load_visible_with_status (registry.rs:1156), list_visible (:635), load_run (:728), cancel_run (:829), and load_files (:1332) each bind parts.user_id as $2 while the SQL only references `a.user_id IS NULL` and never $2 (works only because sqlx declares the param type in Parse). The tenant_id round-trip parse is CONFIRMED (crates/moa-skills/src/registry.rs:383-395 parses storage_partition_id back into a TenantId even though moa.artifact has a real tenant_id column that REVISION_COLUMNS at registry.rs:1345 simply doesn't select). Bonus dead code the claim missed: skill_scope_from_stored_parts (crates/moa-orchestrator/src/services/skills.rs:786) matches scope string "user", but moa.compute_scope_tier (V000001 lines 45-57) generates 'contact', never 'user' — that arm is unreachable.

LOAD-BEARING CONSTRAINT THE CLAIM MISSED — the DB half of the proposal is wrong. The `storage_partition_id + user_id + GENERATED scope` triple is not artifact-specific machinery; it is the repo-wide RLS contract. V000001 lines 77-156 define moa.apply_three_tier_rls, whose CREATE POLICY statements hard-reference the columns `scope`, `storage_partition_id`, and `user_id` (rd_global/rd_tenant/rd_user/wr_tenant/wr_user). It is applied to ~29 tables (sessions, events, node_index, edge_index, embeddings, memory_digests, experience_records, score_run, experiment_run, ...), and the contact tier is genuinely live platform-wide: sessions has user_id NOT NULL (scope computes 'contact'), and crates/moa-brain/tests/brain_db_memory/hybrid_retrieval_db_memory.rs pins user_scope_fact_invisible_to_other_user_at_any_k. Dropping user_id and the generated scope column from the five artifact tables makes `SELECT moa.apply_three_tier_rls('moa.artifact')` (V000001:2422-2426) fail at migration time — the policies reference dropped columns — so the "simplification" forces bespoke tenant-only RLS policies for artifact tables, forking the uniform security pattern. That moves complexity into the security layer rather than deleting it. The claim's own doc citation cuts both ways: docs/09 says skills are tenant-scope, but the schema pattern exists for RLS uniformity, not for skill-tier ambition.

MISSED SIDE EFFECTS: (1) artifact_run_idempotency_uniq and the ON CONFLICT target in append_run (registry.rs:674-679) use `coalesce(user_id,'')` — both index and query must change together; same for experiment_run_idempotency_uniq. (2) Three more tables share the identical triple and FK into artifact tables: analytics.score_run, moa.experiment_run, moa.experiment_run_artifact_revision (V000001:2449-2544) — the claim counted 5 tables. (3) crates/moa-skills/src/review.rs:379 consumes revision.user_id.is_some() in ensure_tenant_skill_draft (defense-in-depth scope validation). (4) services/artifacts.rs:227 and agent_definitions.rs:188 consume summary.scope.

WHAT SURVIVES: the Rust-surface cruft is real and safely deletable because RlsContext::tenant + FORCE ROW LEVEL SECURITY already makes non-tenant rows invisible to every registry query, so the belt-and-suspenders `user_id IS NULL` predicates and all dead $2 binds are redundant with the database policy.

> **Revised simpler alternative:** Keep the DB schema exactly as-is (nullable user_id + GENERATED scope columns are the shared apply_three_tier_rls contract; do not touch V000001). Simplify only the Rust surface: (1) delete the five dead user_id binds ($2 in load_visible_with_status, list_visible, load_run, cancel_run, load_files); (2) collapse ArtifactScopeParts to concrete fields `tenant_id: Uuid, storage_partition_id: String` and bind a literal NULL (or omit the column) for user_id in the three INSERTs — the Option tri-state goes away while the columns stay; (3) add `a.tenant_id` to REVISION_COLUMNS and read it directly instead of parsing TenantId back out of the partition string in skill_from_package_revision; (4) replace Skill.user_id/Skill.scope and StoredArtifactRevision.user_id/scope with a load-time validation that rejects any non-'tenant' scope row (fold ensure_tenant_skill_draft's user_id check into that), and delete the unreachable "user" match arm in skill_scope_from_stored_parts (generated tier is 'contact', never 'user'); (5) optionally drop the redundant `user_id IS NULL` predicates since RLS under RlsContext::tenant already hides contact/global rows — or keep them as documented defense-in-depth, either is defensible.

> **Revised side effects:** No migration edit and no dev-wipe needed (schema untouched). Ripples: crates/moa-orchestrator/src/services/skills.rs (skill_scope_from_stored_parts:776, memory_scope_from_skill:846, reject_user_scoped_skill:772), crates/moa-skills/src/review.rs:377-379 (ensure_tenant_skill_draft), crates/moa-orchestrator/src/services/artifacts.rs:227 and agent_definitions.rs:188 (summary.scope consumers — ArtifactSummary.scope can stay since the DB column stays, or be validated/dropped alongside), revision_from_row/REVISION_COLUMNS lockstep comment at registry.rs:1341-1349, ArtifactScopeParts consumers in moa-agents/src/resolver.rs and moa-skills/src/registry.rs, and _db_memory tests asserting scope strings (e.g. crates/moa-orchestrator/tests/orchestrator_offline/skills_service.rs:56, brain artifact_skill_injection_db_memory). If the redundant `user_id IS NULL` predicates are removed, correctness then depends entirely on RLS policies; any future code path that opens a non-ScopedConn (superuser/owner role, which bypasses FORCE RLS via owner_dev_access) would lose the application-level scope filter.

---

### 41. Lesson-graph subsystem (learn_lesson + render addenda + SkillRegistry::load_full) has zero production callers

**Area:** skills / artifacts
effort: **small** · finder confidence: **high** · ~LOC removable: **~480**

**Locations**

- `crates/moa-skills/src/lessons.rs (whole file, 180 lines)`
- `crates/moa-skills/src/render.rs (whole file, 128 lines)`
- `crates/moa-skills/src/registry.rs:166-182, 313-315 (load_full, tenant_memory_scope)`
- `crates/moa-skills/tests/skills_db_memory/lessons_db_memory.rs, crates/moa-skills/tests/skills_db_memory/render_db_memory.rs`

**What it is.** A complete write-and-read subsystem: learn_lesson opens an RLS-scoped transaction against the memory graph store and writes a Lesson node linked to a skill revision; render.rs queries moa.node_index for Lesson nodes and prepends a '<!-- learned lessons -->' addenda block to SKILL.md; SkillRegistry::load_full wires the two together with a MemoryScope. Both LessonContext and SkillRenderContext carry an assume_app_role test knob and render has a configurable addendum limit.

**Why it may be over-engineered.** No production code writes or reads lessons: the only callers of learn_lesson, render, SkillRenderContext, and load_full are moa-skills' own _db_memory tests — a closed loop that tests only itself. The production skill materialization path (orchestrator services/skills.rs, brain pipeline) uses load_packages_for_scope/load_for_scope and never load_full. docs/09-skills-and-learning.md does not mention lessons at all; its learning story runs through learning_candidates/learning_log. This subsystem is also the only reason moa-skills depends on moa-memory-graph and moa-memory-types.

**Simpler alternative.** Delete lessons.rs, render.rs, SkillRegistry::load_full, tenant_memory_scope, the two test files, and the moa-memory-graph/moa-memory-types dependencies (plus set_app_role in util.rs, which only these use). If per-skill learned addenda are wanted later, the documented mechanism already exists: propose a skill revision through the learning-candidate review flow.

**Side effects / what to watch.** None at runtime — nothing reaches this code. Loses a speculative 'append lessons without a new revision' capability that bypasses the review boundary the docs mandate (arguably a feature to lose). Lesson graph nodes written by old test runs become orphaned NodeLabel::Lesson rows.

**Value of simplifying.** Removes a whole moving part (graph writes + SQL read path + two context structs with role-switching knobs), two crate dependencies, and ~480 LOC; shrinks moa-skills' build graph.

**Adversarial verifier: 🟡 ADJUSTED.** Core claim CONFIRMED as fact; only the deletion checklist is incomplete. Evidence: (1) Zero production callers. Workspace-wide grep + graphify show learn_lesson, LessonContext, render, SkillRenderContext, load_full are referenced only from crates/moa-skills/tests/skills_db_memory/{lessons_db_memory.rs,render_db_memory.rs} and lessons.rs's own inline validation tests. The production skill path is crates/moa-orchestrator/src/services/skills.rs:406 (load_packages_for_scope) and :443 (load_for_scope); moa-brain's SkillInjector (crates/moa-brain/src/pipeline/skills/mod.rs) works from metadata/manifests. Nothing calls render::render or load_full outside the crate. (2) Dependency claim verified: moa-memory-graph/moa-memory-types are non-optional [dependencies] in crates/moa-skills/Cargo.toml (lines 20-21) and their only src uses are lessons.rs, render.rs, and registry.rs:11's MemoryScope import which serves only tenant_memory_scope (registry.rs:313), which serves only load_full. util.rs set_app_role is used only by lessons.rs:75 and render.rs:63; map_sqlx_error must stay (used by proposals.rs:189). (3) No load-bearing constraint: no Restate handler, perf path (docs/18), or security surface reaches this code. docs/09-skills-and-learning.md never mentions lessons; its 'rendering' (line 185) is package rendering/turn-time injection, and the documented learning path (LearningCandidateType::Skill draft proposals, lines 193-209) is the real alternative the claimant cites. (4) Important nuance that does NOT rescue the code: NodeLabel::Lesson is a live production concept elsewhere — the moa-hands memory tool (crates/moa-hands/src/tools/memory.rs:37,124) lets agents write Lesson nodes, brain planner/retrieval query them (crates/moa-brain/src/planning/planner.rs:366, retrieval/legs.rs:760), privacy export includes them in facts.jsonl (crates/moa-orchestrator/src/services/privacy/repository.rs:224), and migrations reference the label. But none of that flows through moa-skills: render.rs filters on properties_summary->>'skill_uid', which only learn_lesson writes, so the skill-linked closed loop stands and the label/consumers survive the deletion untouched. Gap in the proposal: as written it does not compile — tests/support/skill_graph.rs (shared with registry_db_memory.rs via DISTILLED_SKILL/GRAPH_TEST_LOCK/IMPROVED_SKILL/purge_test_skill_name/map_sqlx_error) imports moa_memory_graph/moa_memory_types from the main [dependencies], and lib.rs plus the tests/skills_db_memory.rs harness declare the modules being deleted.

> **Revised simpler alternative:** Delete as proposed (lessons.rs, render.rs, SkillRegistry::load_full, tenant_memory_scope, tests/skills_db_memory/lessons_db_memory.rs and render_db_memory.rs, set_app_role in util.rs, moa-memory-graph + moa-memory-types from Cargo.toml [dependencies]) PLUS the mechanical fallout the claim omitted: remove 'pub mod lessons;' and 'pub mod render;' from crates/moa-skills/src/lib.rs (lines 13, 22); remove the lessons_db_memory/render_db_memory mod declarations from crates/moa-skills/tests/skills_db_memory.rs; remove 'use moa_memory_types::MemoryScope;' from registry.rs:11; and trim tests/support/skill_graph.rs down to the helpers registry_db_memory.rs actually imports (tenant_scope, GRAPH_TEST_LOCK, purge_test_skill_name, map_sqlx_error, DISTILLED_SKILL, IMPROVED_SKILL), deleting graph_store, memory_scope, and its set_app_role, which are used only by the two lesson test files. Keep map_sqlx_error in util.rs (proposals.rs uses it).

> **Revised side effects:** Claimant's 'none at runtime' holds. Two refinements: (1) 'Orphaned NodeLabel::Lesson rows' overstates it — Lesson is a live production label written by the moa-hands memory tool and read by brain retrieval/planner and the GDPR export (facts.jsonl); only skill_uid-linked lesson nodes from old test runs lose their writer/reader, and they remain exportable/erasable through the generic label-based privacy paths. (2) docs/operations/subject-access-runbook.md:66 lists a 'skill_addenda.jsonl' export file that no code produces today (repository.rs writes facts/entities/relationships/embeddings/skills/changelog only) — that runbook line is already stale independent of this change and should be cleaned up alongside it so the deletion doesn't look like it broke a documented compliance artifact.

---

### 42. ProcedureCondition::Expression escape-hatch variant that can only ever fail at runtime

**Area:** skills / artifacts
effort: **small** · finder confidence: **high** · ~LOC removable: **~35**

**Locations**

- `crates/moa-artifacts/src/procedure.rs:128-134`
- `crates/moa-skills/src/procedure/interpreter.rs:682-688`
- `crates/moa-skills/src/procedure/error.rs:99-106 (UnsupportedConditionExpression); error.rs:59-63 (EdgeNotFound, never constructed)`

**What it was.** ProcedureCondition had a third variant, Expression { language, source }, documented as an 'escape hatch for future expression languages'. The interpreter's evaluate_condition unconditionally returned ProcedureError::UnsupportedConditionExpression for it, and artifact validation (validate_procedure in moa-artifacts/src/validation.rs) performed no condition checks — so a procedure using it validated, published, and then deterministically failed mid-run at the first evaluation. The original finding also claimed EdgeNotFound was dead; the verifier proved it is constructed by the orchestrator and must stay.

**Why it was over-engineered.** Speculative extensibility for a feature with no implementation, no plan in docs/09, and no way to succeed: the variant's only effect was moving a parse-time rejection to a worse failure point (a published procedure dying mid-run). Serde's tagged-enum parsing rejects unknown condition types at import time with a clear error, which is better behavior.

**Implemented simplification.** Deleted the Expression variant, the UnsupportedConditionExpression error variant, the interpreter match arm, the orchestrator error mapping arm, and the published JSON-schema expression branch. Kept EdgeNotFound because it is a live error path. When a real expression language lands, add the variant together with its evaluator and a publish-time validation rule in the same change.

**Side effects / what to watch.** Documents containing expression conditions stop parsing (import-time error instead of run-time error) — a clean break the pre-prod policy explicitly permits; no stored procedures can rely on it since it never worked.

**Value of simplifying.** Small LOC but removes a guaranteed-runtime-failure trapdoor from the published-artifact surface and shortens the procedure error enum to variants that can actually occur.

**Adversarial verifier: 🟡 ADJUSTED.** Core claim confirmed for the Expression variant: crates/moa-artifacts/src/procedure.rs:128-134 defines Expression as a documented "escape hatch"; crates/moa-skills/src/procedure/interpreter.rs:682-688 unconditionally errors on it; crates/moa-artifacts/src/validation.rs has zero condition checks (grep for "condition" finds nothing), so a publishing procedure deterministically dies mid-run. The enum uses #[serde(tag = "type")], so after deletion an unknown condition type fails at import-time deserialization — the claimed strictly-better behavior is real. Workspace-wide grep found no test, fixture, doc, or generator that constructs an expression condition, and docs/09-skills-and-learning.md has no expression-language plan. The error maps to a Restate TerminalError(400) in crates/moa-orchestrator/src/workflows/errors.rs:67, so no replay/durability constraint protects it. HOWEVER, the claim is factually wrong about EdgeNotFound: it IS constructed at crates/moa-orchestrator/src/workflows/procedure_execution.rs:1435 in traversed_node_ids() (edge lookup while reconstructing traversed node IDs) and classified in errors.rs:60 — deleting it would break moa-orchestrator compilation and remove a live error path. The claimant also missed two consumers of the Expression variant: docs/schemas/moa-procedure-v1.schema.json publishes "type": "expression" as a oneOf branch of the procedure condition schema (referenced from moa-skill-v1.schema.json:41), and the errors.rs:67 match arm must drop UnsupportedConditionExpression.

> **Revised simpler alternative:** Delete the Expression variant (crates/moa-artifacts/src/procedure.rs:128-134), the interpreter match arm (crates/moa-skills/src/procedure/interpreter.rs:682-688), the UnsupportedConditionExpression error variant (crates/moa-skills/src/procedure/error.rs:99-106), its match arm in crates/moa-orchestrator/src/workflows/errors.rs:67, AND the "expression" oneOf branch in docs/schemas/moa-procedure-v1.schema.json. Do NOT delete EdgeNotFound — it is constructed at crates/moa-orchestrator/src/workflows/procedure_execution.rs:1435 and is a real runtime error path.

> **Revised side effects:** As claimed for Expression: documents with expression conditions fail serde deserialization at import time instead of at runtime (clean pre-prod break; nothing in the workspace uses them). Additional: docs/schemas/moa-procedure-v1.schema.json must drop the expression branch or the published schema will advertise a condition type the parser rejects; errors.rs in moa-orchestrator needs its match arm updated (compile error otherwise). Removing EdgeNotFound would be a genuine breakage, not a cleanup — keep it.

**Implementation status: ✅ DONE.** Current code has no `ProcedureCondition::Expression` or `UnsupportedConditionExpression` references, and `docs/schemas/moa-procedure-v1.schema.json` no longer advertises `"type": "expression"`. Verification passed with `python3 -m json.tool docs/schemas/moa-procedure-v1.schema.json >/dev/null`, `cargo check -p moa-artifacts -p moa-skills -p moa-orchestrator --all-targets --locked`, and `cargo test -p moa-skills --lib procedure --locked`.

---

### 43. MCP credential proxy mints an opaque grant token that is consumed two lines later in the same function

**Area:** edge / messaging / security
effort: **small** · finder confidence: **high** · ~LOC removable: **~250**

**Locations**

- `crates/moa-security/src/mcp_proxy.rs (L16-124)`
- `crates/moa-hands/src/core/dispatch.rs (L399-411)`
- `crates/moa-hands/src/core/construction.rs (L262-273)`

**What it is.** MCPCredentialProxy implements a session-token grant store: create_session_token inserts a ProxyGrant into a moka cache (100k capacity, 15-minute TTL), enrich_headers removes it single-use with a belt-and-suspenders expiry check, and there are revoke_session_token and with_token_ttl APIs plus four tests pinning single-use/TTL/revocation semantics.

**Why it may be over-engineered.** The only production call site (execute_mcp_once in moa-hands dispatch.rs) creates the token and consumes it on the next statement, in the same stack frame. The token never crosses a process, task, await-visible, or trust boundary — it is never given to the model, the sandbox, or another replica. revoke_session_token has zero production callers. The cache, TTL, capacity bound, and revocation exist for a hypothetical future shared durable grant store (docs/08-security.md), i.e. speculative extensibility.

**Simpler alternative.** In execute_mcp_once, replace the mint+consume pair with a direct call: `let credential = vault.get(server_name, server_name).await?; headers_from_credential(Some(credentials), credential)`. Keep headers_from_credential and EnvironmentCredentialVault; delete MCPCredentialProxy, ProxyGrant, McpSessionToken, build_token_cache, and the moka dependency edge in moa-security. The security property (credential resolved host-side at call time, never model-visible) is identical.

**Side effects / what to watch.** docs/08-security.md's 'MCP credential proxy grants are single-use' paragraph must be rewritten to describe the direct vault lookup; four proxy tests are deleted (the header-shaping test survives against headers_from_credential); the ToolRouter loses its mcp_proxy field and with_mcp_proxy builder. If a future design hands grants to untrusted code across requests, the machinery would need to return — but that design requires a durable shared store anyway, which this code explicitly does not have.

**Value of simplifying.** Removes a cache, a TTL clock dependency, and an error path (PermissionDenied on unknown/expired token) from every credentialed MCP tool call; the credential path becomes a plain function call that cannot wedge or mis-expire.

**Adversarial verifier: 🟡 ADJUSTED.** The over-engineering claim is factually correct on every point I could attack. (1) crates/moa-hands/src/core/dispatch.rs L399-411 is the ONLY production caller: execute_mcp_once calls proxy.create_session_token(...) and proxy.enrich_headers(&token, ...) on the immediately following statement, same stack frame; the McpSessionToken never escapes the function, is never journaled by Restate (this runs in moa-hands, not an orchestrator durable step), and is never model- or sandbox-visible. A workspace-wide grep for MCPCredentialProxy/McpSessionToken/create_session_token/enrich_headers/revoke_session_token/with_token_ttl/with_mcp_proxy plus a graphify BFS from those nodes found no other consumers: revoke_session_token has zero production callers (only its own test at mcp_proxy.rs L376), with_token_ttl is used only by the TTL test (L354), and the with_mcp_proxy builder (construction.rs L192) has zero callers anywhere including tests — the proxy is only ever constructed internally in load_mcp_servers (construction.rs L262-273). (2) No hidden consumers in docs/scripts/config beyond the docs/08-security.md L68-72 paragraph the claimant already flagged; that paragraph itself concedes 'Until MOA has a shared durable grant store... code must not expose MCP credential grants across requests', i.e. the single-use/TTL/100k-capacity/revocation machinery enforces an invariant its sole caller cannot violate — the mint+consume pair cannot expire (15-min TTL vs same-statement consumption) and cannot be reused. (3) No load-bearing constraint: no Restate replay/determinism involvement, no perf dependency, and the security property (credential resolved host-side at call time via CredentialVault, never model-visible) is identical under the direct-lookup alternative — with the proxy deleted there is no token to leak at all. moka remains a workspace dep used by moa-brain/moa-ocsf/moa-messaging/moa-skills/moa-memory-ingest; only moa-security's edge is dropped (mcp_proxy.rs is its sole moka use). Adjustments to the proposal: (a) headers_from_credential is a PRIVATE fn in crates/moa-security/src/mcp_proxy.rs L210 — the proposed cross-crate call from moa-hands will not compile until it is made pub (with a doc comment per AGENTS.md rule 1) and re-exported from moa-security's lib.rs; (b) ToolRouter does not hold a vault — the EnvironmentCredentialVault is built inside load_mcp_servers and immediately wrapped in the proxy, so the router's mcp_proxy field must be REPLACED by an mcp_vault: Option<Arc<dyn CredentialVault>> field, not merely deleted; (c) moa-security's lib.rs L12 re-export of MCPCredentialProxy/McpSessionToken and moa-hands' imports (construction.rs L12, mod.rs L23) must be updated. None of these move complexity — the result is a vault handle plus one pub header-shaping function.

> **Revised simpler alternative:** In load_mcp_servers (crates/moa-hands/src/core/construction.rs L262-273), store the EnvironmentCredentialVault directly: replace ToolRouter's mcp_proxy: Option<Arc<MCPCredentialProxy>> field (mod.rs L43) with mcp_vault: Option<Arc<dyn CredentialVault>>. In execute_mcp_once (dispatch.rs L399-411), replace the mint+consume pair with: let credential = vault.get(server_name, server_name).await?; let extra_headers = headers_from_credential(Some(credentials), credential);. Make headers_from_credential pub in moa-security (it is currently private at mcp_proxy.rs L210), add its doc comment, and export it from lib.rs. Delete MCPCredentialProxy, ProxyGrant, McpSessionToken, build_token_cache, the dead with_mcp_proxy builder, and moka from moa-security/Cargo.toml (moka stays a workspace dep for the five other crates using it). Keep EnvironmentCredentialVault, credential_from_env, and headers_from_credential in moa-security.

> **Revised side effects:** All claimed side effects verified, plus: (1) headers_from_credential must change from private to pub (with doc comment per AGENTS.md) and be re-exported from crates/moa-security/src/lib.rs; the claimant's snippet won't compile otherwise. (2) ToolRouter's mcp_proxy field is replaced by an mcp_vault: Option<Arc<dyn CredentialVault>> field, not just removed — the vault currently lives only inside the proxy. (3) moa-security lib.rs L12 re-exports and moa-hands imports (construction.rs L12, mod.rs L23) need updating. (4) Test accounting: four proxy tests (single-use, TTL, revocation, opaque-token header injection) are deleted; the header-shaping assertions from enrich_headers_shapes_api_key_and_oauth_credentials move to a direct headers_from_credential test; the three EnvironmentCredentialVault tests (fail-closed on missing env var, unknown service/scope rejection, env-backed load) survive unchanged. (5) docs/08-security.md L68-72 rewrite as claimed; the 'MCP credential proxy' row in the L60 boundary table (Host enriches MCP calls with real credentials) remains accurate and can stay. (6) with_mcp_proxy has zero callers even in tests, so its removal breaks nothing.

---

### 44. rate_limit.rs ships a full send-retry framework and metrics registry that no production connector uses

**Area:** edge / messaging / security
effort: **small** · finder confidence: **high** · ~LOC removable: **~350**

**Locations**

- `crates/moa-messaging/src/rate_limit.rs (L26-191, L268-350)`
- `crates/moa-messaging/tests/messaging_offline/rate_limiting.rs`

**What it is.** MessagingRateLimiter::send_with_retry is a generic 429-retry loop over a MessagingSendResponse normalization type, with MessagingSendFailure/MessagingFailureClass classification and a MessagingRateLimitMetrics registry (five atomic counters dispatched by string metric name + outcome through async methods).

**Why it may be over-engineered.** Grep-verified: send_with_retry, MessagingSendResponse, MessagingSendFailure, and MessagingFailureClass have zero production callers. Slack uses slack-morphism's built-in rate control plus wait_for_channel_slot; Twilio and Postmark each implement their own retry loop via the shared provider_http helpers and define their own Retryable/Permanent enums (TwilioSmsFailureClass, PostmarkEmailFailureClass, SlackApiFailureClass), making the generic path a fourth parallel classification. The metrics counters are incremented on a path nothing executes and are read only by tests — they are never exported to tracing/OTel; the string-keyed counter_field dispatch and async-over-atomics API exist solely so tests can assert on them.

**Simpler alternative.** Delete send_with_retry, MessagingSendResponse, MessagingSendFailure, MessagingFailureClass, and MessagingRateLimitMetrics; keep wait_for_channel_slot (and its shared-CAS pacing), parse_retry_after, and with_jitter, which are the only production-used pieces. If a shared retry loop is wanted later, grow it out of provider_http where the real connectors already live.

**Side effects / what to watch.** lib.rs re-exports shrink; tests/messaging_offline/rate_limiting.rs loses the retry/metrics tests (the pacing tests stay); the three per-provider failure-class enums remain — optionally unify them into one shared enum as a follow-up rather than keeping four.

**Value of simplifying.** Deletes an entire unused parallel code path (retry loop + normalization type + hand-rolled metrics system) that a reader must currently assume is load-bearing; reduces the crate's public API surface substantially.

**Adversarial verifier: 🟡 ADJUSTED.** Claim is factually accurate and the deletion is safe; only its side-effects list needs correction. Evidence: (1) crates/moa-messaging/src/rate_limit.rs matches the description — MessagingSendResponse L26-98, MessagingFailureClass L100-118, MessagingSendFailure L120-139, MessagingRateLimitMetrics L148-191 (five AtomicU64s dispatched by string (name,outcome) via counter_field, async-over-atomics API), send_with_retry L268-350. (2) Workspace-wide grep (crates, docs, scripts, docker-compose) finds zero production callers: the only references are src/lib.rs re-exports (L38-40), tests/messaging_offline/rate_limiting.rs, and tests/support/rate_limiting.rs; the metric names appear nowhere in docs/ or config and are never exported to tracing/OTel/moa-observability. (3) Slack (src/slack.rs L395/434/632/657) uses the limiter only for wait_for_channel_slot + with_runtime_cache and classifies via its own SlackApiFailureClass (L687) from slack-morphism errors; Twilio (twilio.rs L387) and Postmark (postmark.rs L333) run their own 429 loops with with_jitter and their own TwilioSmsFailureClass/PostmarkEmailFailureClass enums; provider_http.rs uses only parse_retry_after — so the generic path is indeed a fourth parallel, unused classification. (4) No load-bearing constraint: adapter-side code (no Restate replay determinism concern), no doc mandates the counters, AGENTS.md rule 7 and the repo's pre-prod no-compat stance favor deletion. MoaError::HttpStatus stays alive via moa-memory-vector and moa-knowledge. The kept pieces (wait_for_channel_slot with shared-CAS pacing, parse_retry_after, with_jitter) are exactly the production-used surface, so the simpler alternative works. Correction: the claimant's side effects understate test fallout — the pacing test in tests/messaging_offline/rate_limiting.rs drives pacing through send_with_retry and must be rewritten, tests/support/rate_limiting.rs becomes fully dead, an inline test pins the HTTP-date Retry-After path via MessagingSendResponse and needs rewriting against parse_retry_after, and several helper fields/functions become dead code.

> **Revised side effects:** Beyond the claimed lib.rs re-export shrink and loss of retry/metrics tests: (1) tests/messaging_offline/rate_limiting.rs — the pacing test burst_of_concurrent_sends_to_same_channel_serialize_below_per_channel_limit (L59-92) calls send_with_retry, so it must be rewritten to drive wait_for_channel_slot directly rather than "staying"; the truly unaffected pacing tests are the inline shared-runtime-cache tests in rate_limit.rs (L526-589). (2) tests/support/rate_limiting.rs (post_send/mock helpers built around MessagingSendResponse) becomes entirely dead and should be deleted with its #[path] include. (3) The inline test retry_after_accepts_http_date_headers (rate_limit.rs L509-524) pins parse_retry_after's HTTP-date form via failure_for_channel; rewrite it against parse_retry_after directly since Twilio/Postmark/provider_http still use that parser in production. (4) Remove now-dead code in the same pass: MessagingRateLimiter.max_retries + with_max_retries + metrics() + metrics field, is_retryable_status, RATE_LIMIT_FALLBACK_BACKOFF, and MessagingSendResponse::retry_after/retry_after_opt/header. MoaError::HttpStatus and MoaError::RateLimited variants remain used elsewhere (moa-memory-vector, moa-knowledge, slack.rs) and are unaffected. The three per-provider failure-class enums remain, as claimed.

---

### 45. CredentialVault trait carried dead delete/list methods

**Area:** edge / messaging / security
effort: **small** · finder confidence: **high** · ~LOC removable: **~130**

**Locations**

- `crates/moa-core/src/traits/mod.rs (L828-843)`
- `crates/moa-messaging/src/delivery.rs (L349-395)`
- `crates/moa-security/src/mcp_proxy.rs (L163-188)`

**What it was.** The CredentialVault trait required get, set, delete, and list. Both production implementations carried dead deletion/listing surface: EnvironmentDeliveryCredentialVault stubbed delete with a 'read-only' error and implemented list by probing seven env vars; EnvironmentCredentialVault implemented RwLock-guarded delete/list methods that nothing in production invoked. Four test mocks across moa-messaging and moa-security also had to stub both methods.

**Why it was over-engineered.** The delete/list surface was leftover from the removed local encrypted vault (docs/08-security.md: 'Local encrypted vault storage is no longer part of the active runtime'). No production or test code called credential-vault delete/list. The original claim that set was dead was wrong: tenant knowledge account linking uses `CredentialVault::set` to persist exchanged credential material, and later sync/ingestion resolves it through `get`.

**Implemented simplification.** Shrunk the trait to `get` + `set`: deleted `delete` and `list` from CredentialVault, EnvironmentDeliveryCredentialVault, EnvironmentCredentialVault, the security proxy mock, and the messaging offline/live test vaults. Kept `set` and EnvironmentCredentialVault's RwLock because `VaultKnowledgeCredentialStore::store_linked_account` is a production write path.

**Side effects / what to watch.** A future vault deletion/listing UI would re-add those methods together with concrete callers. Test mocks are shorter. The knowledge linked-account path still has write capability through `set`.

**Value of simplifying.** Shrinks a core trait to its actual contract and deletes dead stubs/unreachable list-by-env probing without breaking tenant knowledge credential storage.

**Adversarial verifier: 🟡 ADJUSTED.** The claim's central "grep-verified" fact is wrong for `set`, but right for `delete` and `list`. Evidence: (1) crates/moa-orchestrator/src/services/knowledge/mod.rs L678-694 — production `VaultKnowledgeCredentialStore::store_linked_account` calls `self.vault.set(&service, &scope, Credential::Bearer(...))` (L689-690); the claimant's grep missed it because rustfmt puts `.set(` on its own line, so `vault\.set` never matches. (2) This is reachable from a real production handler: crates/moa-orchestrator/src/services/knowledge/link.rs L60 calls `store_linked_account` when a tenant links a knowledge account (docs/21-tenant-knowledge-base.md flow). (3) The vault wired in is exactly the impl the claim wants gutted: `knowledge_credential_vault()` at mod.rs L924-934 builds a process-global `OnceLock<Arc<dyn CredentialVault>>` holding `moa_security::EnvironmentCredentialVault::from_mcp_servers(&[])`, i.e. an initially-empty vault that only works because runtime `set` populates it; `resolve_linked_account` (mod.rs L696-714) and the ingestion workflow (crates/moa-orchestrator/src/workflows/knowledge_sync_ingestion.rs L610-624) later `get` from it. Because the OnceLock singleton is shared across concurrent async handlers with runtime writes, the RwLock in EnvironmentCredentialVault (crates/moa-security/src/mcp_proxy.rs L127-177) is load-bearing and cannot become a plain HashMap. So "shrink the trait to get-only" would not compile (VaultKnowledgeCredentialStore) and would functionally break knowledge linked-account credential storage — a side effect the claimant missed entirely. However, the claim survives for `delete` and `list`: exhaustive greps across all six impls (crates/moa-messaging/src/delivery.rs, crates/moa-security/src/mcp_proxy.rs, four test mocks in crates/moa-messaging/tests/) and all consumers (moa-hands MCPCredentialProxy only calls `get` at mcp_proxy.rs L102; twilio.rs/postmark.rs from_vault only `get`) found zero callers of `vault.delete` or `vault.list` in production or tests — the slack.rs `.delete(` hits are a different cache object, and the pgvector `.delete(` hits are a vector store. The seven-env-var probing `list` in EnvironmentDeliveryCredentialVault (delivery.rs L361-395) is pure dead weight. Minor accuracy note: the trait (crates/moa-core/src/traits/mod.rs L829-841) uses `StoredCredential`, which is just `Credential as StoredCredential` (L26) — same type.

> **Revised simpler alternative:** Shrink the trait to `get` + `set` only: delete `delete` and `list` from CredentialVault (crates/moa-core/src/traits/mod.rs L836-840), from EnvironmentDeliveryCredentialVault (crates/moa-messaging/src/delivery.rs L355-395, including the seven-env-var probing list), from EnvironmentCredentialVault (crates/moa-security/src/mcp_proxy.rs L171-188), and from the four test mocks in crates/moa-messaging/tests/. Keep `set` and the RwLock in EnvironmentCredentialVault (production write path via VaultKnowledgeCredentialStore); keep EnvironmentDeliveryCredentialVault's read-only `set` error stub.

> **Revised side effects:** The original proposal would break production: crates/moa-orchestrator/src/services/knowledge/link.rs L60 -> store_linked_account -> vault.set is the only way linked-account credential material reaches the vault that sync/ingestion later reads via vault.get; removing `set` (or downgrading EnvironmentCredentialVault's RwLock to a plain HashMap) breaks tenant knowledge-base account linking and fails compilation in moa-orchestrator. The corrected simplification (drop only delete/list) has the side effects the claimant listed, scaled down: each of the four test mocks and both production impls lose 2 methods, not 3; EnvironmentDeliveryCredentialVault's env-probing list still goes away.

**Implementation status: ✅ DONE.** Current `CredentialVault` exposes only `get` and `set`; all credential-vault `delete`/`list` impls are gone. Verification passed with `cargo check -p moa-core -p moa-security -p moa-messaging -p moa-orchestrator --all-targets --locked`, `cargo check -p moa-core -p moa-security -p moa-messaging -p moa-orchestrator --all-targets --all-features --locked`, `cargo test -p moa-security --lib mcp_proxy --locked`, and `cargo test -p moa-messaging --test messaging_offline --locked from_vault_uses`. A mistaken two-filter cargo invocation was rejected before running tests and then rerun with the shared filter.

---

### 46. Slack outbound-ref store hand-rolls a distributed lock plus a two-tier cache for edits that are already serialized upstream

**Area:** edge / messaging / security
effort: **medium** · finder confidence: **low** · ~LOC removable: **~220**

**Locations**

- `crates/moa-messaging/src/slack.rs (L29-45, L53-273, L584-596)`

**What it is.** SlackOutboundMessageRefs keeps multi-chunk message refs in two tiers (process-local moka hot_refs with 100k cap + optional shared RuntimeCacheStore, with best-effort refresh, TTL re-arming on read, and fallback-to-local-on-shared-failure paths). Every edit() and delete() additionally acquires SlackOutboundRefUpdateLock — a hand-rolled distributed mutex built from CAS with uuid tokens, a 120s lock TTL, a jittered 25ms acquire loop with deadline, and release via CAS to a 1ms-TTL tombstone — held across the full multi-call Slack API sequence.

**Why it may be over-engineered.** The lock exists to serialize concurrent cross-replica edits of the same message id, but outbound Slack messages belong to one session, and session work is executed through the session-keyed Restate virtual object, which already serializes per-session handlers — the architecture does not produce two replicas editing one message concurrently. The common single-chunk case needs no stored refs at all (slack_message_id_from_ref round-trips channel:ts from the id). The docs themselves concede the memory-backend mode is 'per-pod best effort', demonstrating correctness does not actually depend on the lock. The result is five failure paths (lock timeout, lost release, CAS contention, shared-read failure fallback, TTL-refresh failure) guarding a race that should be impossible.

**Simpler alternative.** Delete SlackOutboundRefUpdateLock and the hot_refs tier. Store multi-chunk refs as a plain get/set record in the shared runtime cache (or on the session channel binding row in Postgres, which is already the durable routing store), read it at edit/delete time, and overwrite it after the API calls. If the single-writer-per-session invariant is ever intentionally broken, add the lock back with a documented reason.

**Side effects / what to watch.** If some path outside the session VO does concurrently edit one message, last-writer-wins on the ref list (worst case: a shrinking edit leaves one orphan trailing chunk). Three offline tests (cross-instance lock serialization, hot-ref refresh after TTL loss, local-refs-after-shared-failure) are deleted or rewritten. Requires confirming no edit path bypasses the session virtual object before deleting.

**Value of simplifying.** Removes a homemade distributed mutex — the single most failure-prone moving part in the Slack adapter (its acquire timeout surfaces as a user-visible edit error) — plus a redundant cache tier and its reconciliation warnings.

**Adversarial verifier: 🟡 ADJUSTED.** Factual accuracy: verified. crates/moa-messaging/src/slack.rs matches the claim exactly — SlackOutboundMessageRefs (L54-57) keeps a moka hot_refs tier (100k cap, 7-day TTL, L82-88) plus optional shared RuntimeCacheStore with best-effort refresh (refresh_shared_refs_best_effort L258-272), TTL re-arm on read (expire() at L132-141), and fallback-to-local-on-shared-read-failure (L145-155). SlackOutboundRefUpdateLock (L59-77, L185-224) is a hand-rolled CAS mutex with uuid token, 120s TTL, jittered 25ms retry loop, release to a 1ms-TTL tombstone, held across the full edit/delete API sequence (L583-596). The three named offline tests exist verbatim (slack_shared_update_lock_serializes_cross_instance_ref_updates L1503, slack_hot_multi_chunk_refs_refresh_shared_cache_after_ttl_loss L1458, slack_ref_storage_failure_after_side_effect_keeps_local_refs L1429). docs/03-communication-layer.md L196-200 indeed concedes memory-backend mode is "per-pod best effort" and that durable routing comes from Postgres bindings — so no doc mandates the lock.

Consumers: a workspace-wide search found exactly ONE production caller of ChannelAdapter::edit — progress_delivery.rs L229 (send_or_edit_status_message) — and ZERO production callers of ChannelAdapter::delete (only trait impls/tests). The single edited message is the per-turn progress status line, whose summary is capped at 240 chars (MAX_STATUS_SUMMARY_CHARS), i.e. always single-chunk, and single-chunk ids round-trip via slack_message_ref_from_id (resolve() fallback, slack.rs L95-103). So the multi-chunk lock+two-tier machinery currently guards a path with no production multi-chunk edit/delete caller at all — stronger than the claim states.

Serialization premise: docs/02-brain-orchestration.md L82-94 confirms Session VO single-writer-per-key semantics; turns run in TurnExecution keyed by turn_id, one at a time per session ("Session drains the next queued message"), and the status message id lives in per-workflow Restate state (K_PROGRESS_STATUS_MESSAGE), so no two live invocations edit the same message id by design. The one residual concurrency source the claimant missed is a Restate zombie attempt: adapter.edit runs inside ctx.run (progress_delivery.rs L201-218), and after a failover the old attempt's in-flight chat.update can overlap the retried one. But progress delivery is explicitly best-effort (errors swallowed with warn!, failed edit falls back to a replacement send at L229-252), and the message is single-chunk, so the worst case is a momentarily stale status line — the lock buys nothing correctness-critical. The lock also cannot prevent the actual dangerous case (sequential retry after a crash mid-edit re-reading stale refs), since that is not concurrent.

Corrections to the proposal: (1) Drop the Postgres session-channel-binding-row option — refs are keyed per outbound message id and one session has many outbound messages; wrong granularity. The shared runtime-cache plain get/set (which already exists as write_shared_refs/load) is the right target, and production always wires a runtime cache (SlackAdapter::from_config_with_runtime_cache is the only production constructor, moa-orchestrator/src/runtime/deps.rs L273). (2) Do NOT stop recording single-chunk refs on send, or add an id-parse fallback to resolve_target: resolve_target (slack.rs L360-388) resolves reply_to targets via outbound_refs.load() and, unlike resolve(), has no slack_message_ref_from_id fallback — dropping single-chunk storage without that fallback silently degrades reply threading to the coarser channel_ref route. (3) Additional side effects missed: docs/03-communication-layer.md L196-200 ("replicas coordinate ... edit/delete references") must be reworded; the fourth test slack_multi_chunk_refs_survive_adapter_instance_boundaries_with_runtime_cache survives as-is; bare SlackAdapter::new() (no cache) would lose multi-chunk edit continuity, but no production path constructs it that way. compare_and_set stays used by rate_limit.rs, so no trait surface is orphaned.

> **Revised simpler alternative:** Delete SlackOutboundRefUpdateLock (slack.rs L59-77, L185-236) and the hot_refs tier, keeping a single plain get/set record per outbound message id in the shared RuntimeCacheStore (the existing write_shared_refs/load minus TTL re-arm, best-effort-refresh, and local-fallback branches). Do not move refs to the session channel binding row — refs are per-message, not per-session. Keep recording refs on send for both single- and multi-chunk messages (or add the slack_message_ref_from_id parse fallback to resolve_target's reply_to path, which currently lacks it). Note the residual concurrency source in a comment: Restate zombie ctx.run attempts in progress_delivery can briefly overlap edits of the same status message; this is acceptable because progress delivery is best-effort, single-chunk, and already falls back to a replacement send on edit failure.

> **Revised side effects:** In addition to the claimant's list: (1) docs/03-communication-layer.md L196-200 must be updated (it documents replica coordination of edit/delete references). (2) resolve_target (slack.rs L360-388) consumes the ref store for reply_to routing and has no id-parse fallback — dropping single-chunk ref storage without adding that fallback silently downgrades outbound reply threading to the channel_ref route. (3) Bare SlackAdapter::new() without a runtime cache loses multi-chunk edit/delete continuity entirely once hot_refs is gone; only tests construct it that way (production uses from_config_with_runtime_cache, runtime/deps.rs L273). (4) Restate zombie invocation attempts (old ctx.run still in flight during failover retry) are the one real cross-process concurrency; consequence is a momentarily stale single-chunk status line, not ref corruption, since no production path edits or deletes multi-chunk messages today. (5) Losing the shared-read-failure local fallback means a Redis blip fails the edit; progress_delivery already handles that by sending a replacement status message.

---

### 47. OrchestratorCtx dependency-group pyramid: 10-way trait-object fan-out of one store plus three parallel accessor surfaces, mostly dead

**Area:** Orchestrator — services/rest
effort: **medium** · finder confidence: **high** · ~LOC removable: **~400**

**Locations**

- `crates/moa-orchestrator/src/ctx.rs:26-587`
- `crates/moa-orchestrator/src/runtime/deps.rs:178-199`

**What it is.** ctx.rs wraps every runtime dependency in seven 'dep group' structs (PersistenceDeps, AuthDeps, ProviderDeps, ToolDeps, MemoryDeps, LineageDeps, MessagingDeps), each with private fields and trivial clone-getters. PersistenceDeps holds 10 separate Arc<dyn Trait> fields (session_repository, session_store, segment_store, experience_store, learning_candidate_store, analytics_store, event_lookup_store, learning_log_store, channel_store, action_policy_store) that are ALL clones of the same single Arc<PostgresSessionStore>, plus the concrete backend and pool, each with its own getter. OrchestratorCtx then exposes three parallel accessor surfaces: static current_*() helpers, instance getters, and *_deps() group getters (~40 methods).

**Why it may be over-engineered.** PostgresSessionStore is the only implementor of every one of those narrow traits (grep 'impl X for' finds exactly one impl each, no test doubles), and SessionRepository is a blanket supertrait (moa-core/src/traits/mod.rs:449-461) that already grants all of them. Verified call-site counts outside ctx.rs across the whole workspace: segment_store/experience_store/analytics_store/event_lookup_store/learning_log_store/channel_store/session_event_store/action_policy_store getters = 0 uses; learning_candidate_store = 1; all six *_deps() group getters = 0 uses; lineage_writer(), runtime_cache()/current_runtime_cache(), channel_adapter() = 0 uses (current_channel_adapter used once). Real traffic goes through only current_graph_pool (112), current_session_store (30), session_store_backend (6), and a handful of others. The grouping+getter ceremony is ~450 lines that route one Arc through eleven names.

**Simpler alternative.** Flatten OrchestratorCtx to a single struct with pub fields: config, session_store: Arc<PostgresSessionStore>, pool, fga_client, auth_providers, runtime_cache, providers, embedding_provider, tool_router, tool_schemas, graph_memory_retriever, skill_injector, lineage handle+writer, channel_adapters. Keep only the static current_* helpers that are actually called (current_graph_pool, current_session_store, current_config, current_provider_registry, current_tool_router/schemas, current_lineage, current_channel_adapter). Callers needing a narrow trait object coerce the one Arc at the use site (Arc<PostgresSessionStore> already impls SessionRepository via the blanket impl). Delete the seven group structs, OrchestratorDeps, and all unused getters; RuntimeDeps::orchestrator_ctx() becomes a plain struct literal.

**Side effects / what to watch.** Mechanical churn in runtime/deps.rs and the few real call sites. crates/xtask/src/check_architecture_boundaries.rs has counted allowances keyed on accessor names like 'OrchestratorCtx::current_graph_pool' and 'grouped deps' reasons — its allowance table must be updated in the same change. No behavior or persistence change; no test doubles exist to break. Another agent is concurrently editing this crate, so coordinate before applying.

**Value of simplifying.** Deletes ~400 lines of pure indirection in the crate's most-imported module, removes eleven aliases for one object, and makes 'where does a handler get X' answerable in one hop instead of three.

**Adversarial verifier: 🟡 ADJUSTED.** The dependency pyramid is real (`crates/moa-orchestrator/src/ctx.rs:26`, `:314`, `:444`; `runtime/deps.rs:178`), but the proposal should not flatten all consumers to raw `Arc<PostgresSessionStore>`. Keep the `SessionRepository` seam (`ctx.rs:390`, `moa-core/src/traits/mod.rs:448`) and collapse redundant `PersistenceDeps` facet fields plus unused group getters.

---

### 49. SessionStore service '_inner' layer: ~28 one-line pass-through methods between handlers and the store

**Area:** Orchestrator — services/rest
effort: **small** · finder confidence: **high** · ~LOC removable: **~300**

**Locations**

- `crates/moa-orchestrator/src/services/session_store/inner.rs:203-503`
- `crates/moa-orchestrator/src/services/session_store/handlers.rs`

**What it is.** Every RestateSessionStore handler in handlers.rs clones the service and calls a matching *_inner method on SessionStoreImpl inside ctx.run(). inner.rs defines ~28 such methods (append_event_inner, get_events_inner, get_session_inner, update_status_inner, create_segment_inner, ... record_segment_turn_usage_inner), and all but two are exactly `self.store.method(args).await.map_err(HandlerError::from)` — a third name for the same operation that already exists on the SessionStore trait and on the Restate handler.

**Why it may be over-engineered.** The middle layer adds no authz, no validation, no journaling, no test seam (only create_session_inner is #[cfg(test)], and the module's real logic — create_session_for_identity, agent policy application — is separate free functions that would stay). It is an A-calls-B-calls-C chain where B contributes one map_err. Exceptions with actual content: append_event_inner (one metrics line) and get_learning_candidate_inner (404 mapping) — both trivially inlinable.

**Simpler alternative.** Inline the store call into each handler's ctx.run closure: `let store = self.store.clone(); ctx.run(|| async move { store.get_events(...).await.map(Json::from).map_err(HandlerError::from) })`. Keep inner.rs only for the functions with real logic (session creation + authz outbox tuples, agent model policy), or fold those into the module root.

**Side effects / what to watch.** services/session_store/tests.rs uses create_session_inner; switch it to the store directly. Purely mechanical otherwise; the Restate wire surface and durability semantics are unchanged.

**Value of simplifying.** Deletes ~300 lines of pure delegation and removes one of three names per operation, so tracing a session-store call is handler -> store instead of handler -> inner -> store.

**Adversarial verifier: 🟡 ADJUSTED.** The `_inner` delegation layer is real (`handlers.rs:119`, `inner.rs:215`), but there are more exceptions than the raw note lists, including tenant-id string adaptation and 404 mapping (`inner.rs:341`, `:424`). Inline pure delegates, but keep session creation/authz outbox and agent-policy helpers (`inner.rs:13`, `:83`) and retarget tests that currently call multiple `_inner` methods.

---

### 50. The 'redis' cargo feature is mandatory-in-practice: every build enables it and a non-redis binary refuses to start

**Area:** Orchestrator — services/rest
effort: **small** · finder confidence: **high** · ~LOC removable: **~25 (plus build-config lines)**

**Locations**

- `crates/moa-orchestrator/src/runtime/deps.rs:249-262`
- `crates/moa-orchestrator/Cargo.toml:17`
- `Dockerfile:7`
- `docker-compose.yml:205`

**What it is.** moa-orchestrator gates its Redis runtime-cache backend behind a `redis` feature: deps.rs has a #[cfg(feature = "redis")] / #[cfg(not(...))] pair where the not-arm just bails at startup ('runtime_cache.backend = redis requires the moa-orchestrator redis feature'), and build_runtime_cache_store separately rejects the memory backend entirely.

**Why it may be over-engineered.** The orchestrator cannot run without Redis by its own design (memory backend is rejected as process-local), and every real build config already enables the feature: Dockerfile ARG default 'redis', docker-compose 'redis,provider-overrides', run-clean-e2e ORCH_FEATURES 'provider-overrides,skill-learning,redis'. The only thing the flag can produce is a binary variant that always fails at boot — a phantom configuration that still costs a cfg split, a dead code path, and a feature to remember in every script.

**Simpler alternative.** Make moa-runtime-store's redis capability a normal (non-optional) dependency for moa-orchestrator, delete the redis feature from Cargo.toml and the #[cfg] pair in deps.rs, and drop 'redis' from Dockerfile/compose/scripts feature lists.

**Side effects / what to watch.** Slightly larger compile for hypothetical redis-less builds of the crate (none exist). Feature lists in Dockerfile, docker-compose.yml, and scripts/run-clean-e2e.sh need the token removed. No runtime behavior change.

**Value of simplifying.** Removes a build variant that can only fail, one cfg branch, and a startup bail path; one fewer feature token to thread through four build entry points.

**Adversarial verifier: 🟡 ADJUSTED.** Runtime behavior is confirmed: orchestrator rejects memory cache and requires Redis (`runtime/deps.rs:216`, `:249`). Correction: Redis appears in more build paths than listed, including Docker, compose, scripts, deploy, and the test fixture. Make Redis unconditional for `moa-orchestrator`, remove the feature token from binary/deploy/fixture builds, keep memory cache for non-orchestrator tests, and update docs that still describe memory as an orchestrator option.

---

### 51. postmark/twilio feature flags are enabled by no build and guard ~1,450 lines of unreachable adapter code

**Area:** Orchestrator — services/rest
effort: **small** · finder confidence: **medium** · ~LOC removable: **~1450 (option b)**

**Locations**

- `crates/moa-orchestrator/Cargo.toml:15-16`
- `crates/moa-messaging/src/postmark.rs`
- `crates/moa-messaging/src/twilio.rs`
- `crates/moa-messaging/Cargo.toml:8-11`

**What it is.** moa-orchestrator declares `postmark = ["moa-messaging/postmark"]` and `twilio = ["moa-messaging/twilio"]`. The workspace pins moa-messaging with default-features = false, so postmark.rs (685 lines) and twilio.rs (750 lines) plus the cfg-gated branches in delivery.rs compile only when those orchestrator features are enabled — and nothing enables them: not the Dockerfile, docker-compose, Makefile, or any test script, and no orchestrator source file references postmark/twilio at all (build_channel_adapters constructs only Slack). PostmarkEmailClient/TwilioSmsClient are constructed nowhere outside moa-messaging.

**Why it may be over-engineered.** This is fully plumbed speculative capability: two transport clients with retry/failure-classification logic, feature wiring across three Cargo.tomls, and config fields (moa-core config/messaging.rs) that no artifact can ever reach. docs/03-communication-layer.md describes Postmark/Twilio contact-point verification as designed behavior, but in a pre-production repo with no compat requirement, code that no build compiles is dead weight that still gets maintained through every refactor (e.g., moa-messaging API changes must keep these compiling under --all-features CI, if any).

**Simpler alternative.** Pick one: (a) if email/SMS verification is near-term, enable the features in the real build (Dockerfile/compose) and wire ProviderDeliverySink construction in the orchestrator so the code is actually exercised; or (b) delete postmark.rs, twilio.rs, their delivery.rs branches, the three feature declarations, and the unused messaging config fields — they can be restored from git when the channel ships.

**Side effects / what to watch.** Option (b) removes a documented-but-dormant capability; docs/03 would need a note that email/SMS channels are not yet implemented. moa-contacts only uses the sink trait, so it is unaffected either way. Verify no --all-features CI lane exercises these modules' unit tests before deleting.

**Value of simplifying.** Either turns ~1,450 lines of dark code into tested production code or deletes it; eliminates two feature flags and dead config knobs (postmark_base_url, twilio base URL, etc.) that currently exist only to be kept compiling.

**Adversarial verifier: 🟡 ADJUSTED.** The feature-disablement claim mostly holds, but contact verification already reaches `ProviderDeliverySink::from_env()` (`moa-orchestrator/src/services/contacts.rs:206`, `moa-contacts/src/repository.rs:375`) and currently falls into disabled-support branches. Either enable `postmark,twilio` in shipped builds with credentials, or delete the capability and update docs/tests/contact-verification behavior.

---

### 53. Per-turn lineage emits a constant 'retrieval_recall_proxy' score and records cost three different ways

**Area:** moa-brain (context pipeline)
effort: **small** · finder confidence: **medium** · ~LOC removable: **~80**

**Locations**

- `crates/moa-brain/src/lineage.rs:58-80`
- `crates/moa-brain/src/lineage.rs:201-228`
- `crates/moa-brain/src/lineage.rs:336-341`

**What it is.** emit_context_lineage builds a ScoreRecord named `retrieval_recall_proxy` whose value is 1.0 if `chunks_in_window` is non-empty else 0.0 — but chunks_in_window is ALL compiled context messages (every message becomes a chunk), and the pipeline always emits at least the identity system message, so the score is constant 1.0 on every production turn (called from moa-orchestrator/src/brain_bridge.rs:161). It is labeled source=OnlineJudge, evaluator="context-compiler". emit_generation_lineage then records the turn cost three ways: as GenerationLineage.cost_micros in the durable lineage record, again as a separate durable ScoreRecord named `cost_micros` with the identical value, and again as the `moa_cost_micros_per_turn` metrics gauge. emit_citation_scores also SETs the `moa_grounding_verified_rate` gauge to 1.0/0.0 per citation source, so the 'rate' is just the last citation's boolean.

**Why it may be over-engineered.** These are speculative eval-plumbing records emitted durably on the hot turn path that carry zero or duplicated information: a recall proxy that cannot vary, a score row duplicating a field of the record written immediately before it in the same stream, and a gauge whose last-write-wins semantics contradict its name. Each is an extra durable write (record_durable awaits) per turn.

**Simpler alternative.** Delete the retrieval_recall_proxy ScoreRecord (or compute a real signal — e.g. count of citable/tool chunks — only if something consumes it), delete the cost_micros ScoreRecord (cost already lives on GenerationLineage and the gauge), and replace the per-citation gauge with a counter pair (verified/total) if a rate is wanted. ~80 lines and 2+ durable writes per turn removed.

**Side effects / what to watch.** Any lineage-file inspection or dashboard querying score names `retrieval_recall_proxy`/`cost_micros` loses those rows (pre-prod, no external consumers; docs/16 only says lineage files are inspectable). Grounding-rate dashboards would switch to the counter form.

**Value of simplifying.** Fewer durable writes per turn, and the online-score stream stops containing constants that would mislead anyone evaluating retrieval quality from it.

**Adversarial verifier: 🟡 ADJUSTED.** The constant proxy and duplicate cost recording exist (`moa-brain/src/lineage.rs:21`, `:58`, `:184`, `:201`, `:223`), but `retrieval_recall_proxy` uses nonblocking `record`, not awaited `record_durable`. Delete or replace the proxy with a real retrieval signal; keep `GenerationLineage.cost_micros` and the metric, and drop the duplicate score only with an analytics migration note.

---

### 54. Process-wide circuit-breaker registry (HashMap by namespace) with exactly one production namespace

**Area:** moa-brain (context pipeline)
effort: **small** · finder confidence: **high** · ~LOC removable: **~25**

**Locations**

- `crates/moa-brain/src/pipeline/query_rewrite/mod.rs:51-110`
- `crates/moa-brain/src/pipeline/builder.rs:204-215`

**What it is.** QueryRewriter::new_with_shared_circuit looks up an Arc<CircuitBreaker> in a `static SHARED_CIRCUIT_BREAKERS: OnceLock<Mutex<HashMap<String, Arc<CircuitBreaker>>>>` keyed by a caller-supplied namespace string. The only production call site is the pipeline builder, which always passes the literal "default_pipeline". The keyed-registry generality exists so per-turn pipeline rebuilds reuse breaker state, but only one key can ever occur.

**Why it may be over-engineered.** A string-keyed registry plus Mutex for a set of size one is speculative extensibility; the namespace parameter invites divergent breaker states that nothing needs. The same reuse-across-rebuilds property comes from a single static, or better, from owning the breaker where other shared per-process components already live (the builder already threads shared_graph_memory_retriever and shared_skill_injector through GraphMemoryPipelineOptions).

**Simpler alternative.** Replace the registry with a single `static SHARED: OnceLock<Arc<CircuitBreaker>>` initialized from config, or add an optional `shared_query_rewrite_breaker` to GraphMemoryPipelineOptions following the existing shared-component pattern, and delete new_with_shared_circuit's namespace parameter and the HashMap/Mutex.

**Side effects / what to watch.** The cross-instance trip test (mod.rs:691) updates to the new constructor; note the existing subtlety that config changes after first init are ignored — a single static makes that explicit instead of hidden per-key.

**Value of simplifying.** Removes a Mutex-guarded global map and an API parameter whose only value is a hardcoded literal; ~25 lines.

**Adversarial verifier: 🟡 ADJUSTED.** The global breaker registry exists and production only uses `default_pipeline` (`query_rewrite/mod.rs:50`, `:94`; `builder.rs:207`). Correction: the key also includes threshold/window/cooldown config (`query_rewrite/mod.rs:97`), so a single static breaker would collapse distinct configs. Prefer injecting an optional shared breaker through `GraphMemoryPipelineOptions`.

---

### 55. Turbopuffer second vector backend with outbox, promotion state machine, and dual-read for zero tenants

**Area:** moa-memory (graph/ingest/lifecycle/pii/vector)
effort: **large** · finder confidence: **medium** · ~LOC removable: **~2200**

**Locations**

- `crates/moa-memory/vector/src/turbopuffer.rs`
- `crates/moa-memory/vector/src/backend.rs`
- `crates/moa-memory/vector/src/sync.rs`
- `crates/moa-memory/vector/src/promotion.rs`
- `crates/moa-brain/src/retrieval/hybrid.rs:445-670`
- `crates/moa-orchestrator/src/services/admin_maintenance.rs`
- `crates/moa-orchestrator/src/services/graph_memory_maint.rs`

**What it is.** A complete opt-in external vector backend: TurbopufferStore HTTP client with BM25 and BAA gating (768 LOC), a moa.vector_sync_outbox table with claim-lease/retry drain machinery (sync.rs + ~350 LOC of backend.rs), a per-partition promotion state machine (pgvector -> migrating -> dual_read -> turbopuffer) with top-K overlap validation, finalize/rollback, and a dual-read window (promotion.rs), dual-read candidate merging in moa-brain hybrid retrieval, two orchestrator maintenance services (promote/finalize/rollback + outbox drain), an operations runbook, and TransactionalGraphVectorBackend/VectorPostCommitSync wrappers threaded through PostgresGraphStore, slow_path, fast_path, consolidate, and knowledge ingest. Every transactional vector upsert/delete executes an extra outbox INSERT ... WHERE EXISTS statement even on pgvector-only partitions, and callers do a partition_uses_external_backend SELECT to decide whether to drain post-commit.

**Why it may be over-engineered.** MOA is pre-production with no tenants. No storage partition is configured for turbopuffer anywhere (default is pgvector; the only way to flip it is the unused admin promotion endpoint; .env.example ships an empty MOA_TURBOPUFFER_API_KEY). The entire stack — second backend, HIPAA/BAA gate, durable sync outbox, dual-read validation window — exists for a hypothetical 'large or isolation-sensitive tenant' (docs/04). It is speculative extensibility that adds a background moving part (outbox drain), two admin services, three storage_partition_state columns, one table, and a per-write SQL statement to the hot graph-write path.

**Simpler alternative.** Delete TurbopufferStore, promotion.rs, sync.rs, the vector_sync_outbox table, the dual-read leg in hybrid.rs, the two orchestrator maintenance surfaces, and the VectorPostCommitSync/TransactionalGraphVectorBackend wrappers; have graph writes use PgvectorStore directly (the VectorStore trait can stay for the Noop/test impls). Edit the baseline migration in place (allowed pre-prod). Re-introduce an external backend behind the existing VectorStore trait when a real tenant needs it.

**Side effects / what to watch.** Loses the documented Turbopuffer opt-in path, the tenant-vector-promotion runbook, ~10 related error variants, and the turbopuffer offline/live tests plus dual-read and outbox _db_memory tests. Retrieval isolation for large tenants would need to be rebuilt later (the VectorStore seam makes that tractable). This is documented architecture in docs/04, so the owner may consider it deliberate — but nothing in docs/18-performance.md justifies it and no measurement or tenant demands it today.

**Value of simplifying.** Removes ~2,200 lines of production code, one Postgres table, three state columns, two Restate services, a background drain, and one SQL statement from every transactional vector write; eliminates the largest untested-in-production failure surface in the memory subsystem.

**Adversarial verifier: 🟡 ADJUSTED.** The Turbopuffer outbox/promotion/dual-read machinery is real, but it is wired into edge routes, Restate cron/bindings, docs, and a runbook (`moa-edge/src/routes/analytics.rs:460`, `moa-orchestrator/src/runtime/jobs.rs:152`, `docs/04-memory-architecture.md:71`). Delete only if product direction drops the documented opt-in external vector backend, and do not remove unrelated admin/graph-maintenance responsibilities.

---

### 56. Ingestion dependency wiring: process-global runtime with fingerprint compatibility checking plus three parallel dependency bundles and dead entry points

**Area:** moa-memory (graph/ingest/lifecycle/pii/vector)
effort: **medium** · finder confidence: **high** · ~LOC removable: **~400**

**Locations**

- `crates/moa-memory/ingest/src/ctx.rs:22-93,192-418,527-587`
- `crates/moa-memory/ingest/src/slow_path.rs:196-310`
- `crates/moa-memory/ingest/src/fast_path.rs:751-780`

**What it is.** Ingestion dependencies are described by three near-identical bundles: IngestRuntime (global, with pii/embedder/extractor/resolver/detector/vector-factory plus with_* builders), IngestCtx (same members plus duplicated with_* builders), and DirectIngestDeps (private third copy). IngestRuntime lives in a OnceLock service-locator guarded by IngestRuntimeFingerprint — Debug-format hashes of pool options, the Cohere API key, and the memory config, plus name strings ('heuristic'/'llm'/'custom') and summary() formatting — solely to error on 'incompatible reinstall'. The pipeline body (chunk -> extract -> classify -> embed -> contradict -> apply) is written out three times: the Restate VO handler, ingest_turn_direct_with_pool_and_pii, and ingest_turn_direct_with_ctx. ingest_turn_direct has zero callers; ingest_turn_direct_with_pool has one (a live test). fast_path's runtime_fast_ctx also ignores the runtime's cached shared embedder/PII client and rebuilds fresh HTTP clients per memory-tool call, defeating the global's stated purpose.

**Why it may be over-engineered.** install_runtime is called from exactly one production site (orchestrator runtime/deps.rs), so the incompatible-reinstall scenario the fingerprint machinery defends against cannot occur outside a single test binary. The global service locator is not forced by Restate: the same endpoint file already constructs ToolExecutorImpl::new(state).serve(), so IngestionVOImpl can hold its dependencies. Three bundles + duplicated builder methods + a dead public entry point are pure duplication.

**Simpler alternative.** Keep one bundle (IngestCtx). Give IngestionVOImpl a constructor field holding it (built once in runtime/deps.rs from MoaConfig), pass it to the fast-path MemoryToolExecutor the same way, and keep ingest_turn_direct_with_ctx as the single direct entry point (the one live test can build an IngestCtx from config). Delete INGEST_RUNTIME, IngestRuntimeFingerprint, IngestRuntimeInstallError, hash_debug, install_runtime*/current_runtime, DirectIngestDeps, ingest_turn_direct, and ingest_turn_direct_with_pool.

**Side effects / what to watch.** orchestrator runtime/deps.rs and endpoint.rs change to constructor injection; the three ctx.rs unit tests pinning fingerprint/install behavior are deleted; fast-path memory tools need the ctx plumbed through their executor construction. The Restate step journal (ctx.run per step) is untouched.

**Value of simplifying.** Deletes ~400 lines and removes a global mutable service locator plus a hand-rolled config-hashing subsystem; one obvious way to construct ingestion dependencies instead of three drifting ones (fast_path already drifted by rebuilding HTTP clients per call).

**Adversarial verifier: 🟡 ADJUSTED.** The process-global runtime and dependency-bundle duplication are real, but fast memory tools still call the ingest fast path from `ToolExecutorImpl` (`tool_executor.rs:111`) and `runtime_fast_ctx` rebuilds clients (`fast_path.rs:751`). Use one injected bundle for both `IngestionVOImpl` and fast memory tool execution, preserving Restate step journaling and the direct-ingest advisory fence.

---

### 57. embedding_model_version: a parallel identity column that is written everywhere and read nowhere

**Area:** moa-memory (graph/ingest/lifecycle/pii/vector)
effort: **medium** · finder confidence: **high** · ~LOC removable: **~150**

**Locations**

- `crates/moa-memory/vector/src/lib.rs:236-238`
- `crates/moa-memory/vector/src/pgvector_store.rs:360-385,402-440`
- `crates/moa-memory/graph/src/node.rs:200-263`
- `crates/moa-memory/ingest/src/slow_path.rs:492-531`
- `crates/moa-core/src/traits/embedding.rs:16-19`
- `crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql:1005,1125,1138`

**What it is.** An i32 'embedding model version for dual-write upgrades' is threaded through EmbeddingProvider::model_version() (Cohere returns 1, Gemini 2), VectorItem, NodeWriteIntent, NodeEmbeddingIntent, EmbeddedFact, EmbeddingRow, fast_path, slow_path, consolidate entity backfill, knowledge ingestion, turbopuffer attributes, sync/promotion SELECT lists, and three DB columns (moa.embeddings, moa.storage_partition_state, knowledge). Grepping every consumer shows it is only ever written, copied row-to-row, or dumped into the privacy-export JSON — no query filters on it, no code branches on it, and the partition write guard compares the embedding_model string, never the version.

**Why it may be over-engineered.** The machinery exists for 'dual-write upgrades' that cannot happen: guard_storage_partition_embedder_for_write hard-rejects mixing models in a partition, and docs/04 says switching embedders requires full re-embedding. The embedding_model string (e.g. 'embed-v4.0', 'gemini-embedding-2') already uniquely identifies the vector space; a model revision would ship as a new model id. This is a second identity dimension kept 'just in case', and pre-prod there is no compatibility reason to keep the columns.

**Simpler alternative.** Delete the field from all structs and SQL, drop the three columns by editing the baseline migration in place, and remove EmbeddingProvider::model_version(). Keep only the embedding_model string.

**Side effects / what to watch.** Touches ~10 files across moa-memory, moa-core, moa-providers, moa-knowledge, moa-orchestrator privacy export, and one migration; a dev-wipe or targeted ALTERs on the live compose DB (known workflow per project memory). Privacy export JSON loses one meaningless field.

**Value of simplifying.** Removes a dead column from every embedding row and a dead trait method, shrinks every vector write/read struct, and eliminates a concept future readers must (wrongly) assume is load-bearing.

**Adversarial verifier: 🟡 ADJUSTED.** Runtime compatibility gates ignore `embedding_model_version` (`embedding.rs:16`; `pgvector_store.rs:280`, `:300`, `:331`), but privacy export and eval/loadtest provenance read/report it. Removal still holds if those schemas, baselines, and export shapes are updated.

---

### 60. memory.auto_bootstrap config knob has no production reader

**Area:** moa-memory (graph/ingest/lifecycle/pii/vector)
effort: **small** · finder confidence: **high** · ~LOC removable: **~15**

**Locations**

- `crates/moa-core/src/config/memory.rs:8-13,26-28`
- `crates/moa-core/src/config/env_overlay.rs:178-181`

**What it is.** MemoryConfig.auto_bootstrap ('Automatically bootstrap tenant-visible memory when it is empty', default true) is defined in config, mapped from the MOA_MEMORY_AUTO_BOOTSTRAP env overlay, and set to false by two moa-brain test fixtures — but no src/ code anywhere in the workspace reads the value; grep over all crate src trees finds zero consumers.

**Why it may be over-engineered.** It is a dead knob: whatever bootstrap behavior it once gated no longer consults it, so the option, its env-var plumbing, and the test fixtures toggling it are inert configuration surface that misleads operators into thinking they can turn something off.

**Simpler alternative.** Delete the field, its Default entry, the env-overlay mapping, and the two test-fixture assignments; re-add a knob if/when a bootstrap path actually branches on one.

**Side effects / what to watch.** MOA_MEMORY_AUTO_BOOTSTRAP stops parsing (it already did nothing); any TOML setting it would need removal — none exists in the repo.

**Value of simplifying.** Tiny deletion, but every dead config knob is a debugging trap; this one claims to control tenant memory bootstrap and controls nothing.

**Adversarial verifier: 🟡 ADJUSTED.** The knob is inert in production; hits are config/env overlay, `.env.example`, and two brain tests. Correction: `.env.example:44` advertises it too, so remove the sample entry alongside inert config/test wiring.

---

### 62. 119 of 245 env config knobs are never set or referenced anywhere in the repo

**Area:** moa-core (traits/types/config/wire)
effort: **small** · finder confidence: **high** · ~LOC removable: **~450**

**Locations**

- `crates/moa-core/src/config/env_overlay.rs:22-519`

**What it is.** MoaEnvOverlay mirrors nearly every MoaConfig field 1:1 as an env knob. Cross-referencing all 245 `MOA_*` names against the entire repo (docker-compose*, k8s/, docker/, Makefile, scripts/, ops/, fly.toml, services/, all crates incl. tests, docs/, .env.example) shows 119 knobs appear ONLY inside env_overlay.rs itself — e.g. all 9 MOA_QUERY_REWRITE_*, all 8 MOA_RESOLUTION_* weights/thresholds, all 8 MOA_COMPACTION_*, 6 MOA_REDUCTO_*/5 MOA_LLAMAPARSE_* webhook/tier knobs, MOA_DATABASE_MAX_CONNECTIONS, MOA_PERMISSIONS_*, MOA_OBSERVABILITY_LINEAGE_BATCH_*, and the MOA_SESSION_LIMITS_* narration tunables. Since env is the only config source, these are knobs that have never once been turned.

**Why it may be over-engineered.** Speculative configurability: each unused knob costs an overlay field + doc comment + apply line + test surface, purely in case someone someday wants to tune it from Kubernetes. Tests that need non-default values already construct MoaConfig structs directly in code, so the env knob adds nothing for them.

**Simpler alternative.** Delete the 119 overlay fields and their apply/validate lines; keep the underlying MoaConfig fields (code still reads them and tests still set them on the struct). Re-add an env knob only when a deployment actually needs to set it — a 2-line change under the current design, a 0-line change if the derived-env finding is adopted.

**Side effects / what to watch.** None observable: no deployment file, script, doc, or test sets any of these variables. Risk is only an operator relying on an undocumented local-only env var; pre-prod that is acceptable.

**Value of simplifying.** Removes ~450 lines and nearly halves the env surface that must be kept in sync, audited for secrets handling, and documented; shrinks the blast radius of the overlay drift trap even if the bigger rewrite is deferred.

**Adversarial verifier: 🟡 ADJUSTED.** The 245-field and 119-unused counts reproduce when the audit file is excluded, but the exact unused groups differ from the examples and some wildcard docs exist. Trim using a generated exact-name check that excludes `docs/simplification-audit.md`, rather than hand-maintaining the raw list.

---

### 65. EmbeddingProvider carries synonym method pairs (dimensions/dimension, model_id/model_name)

**Area:** moa-core (traits/types/config/wire)
effort: **small** · finder confidence: **high** · ~LOC removable: **~15**

**Locations**

- `crates/moa-core/src/traits/embedding.rs:9-33`

**What it was.** The trait defined required `model_id()` and `dimensions()`, then added defaulted `model_name()` (returns model_id) and `dimension()` (returns dimensions) — pure aliases. Both names of each pair were actively called across moa-memory, moa-knowledge, moa-eval, and moa-loadtest, so the codebase was split between the synonyms.

**Why it was over-engineered.** The aliases existed only to preserve old call-site spellings, which AGENTS.md rule 7 explicitly forbids (no wrapper functions to preserve old paths) and pre-prod status makes unnecessary. Two names for one concept invited divergence: an implementor overriding one alias but not its twin would silently desynchronize them.

**Implemented simplification.** Kept exactly `model_id()`, `dimensions()`, and `model_version()`; deleted `model_name()` and `dimension()` and mechanically updated embedding-provider call sites to the canonical names. Unrelated `VectorStore::dimension()` and citation `model_name()` helpers were left alone.

**Side effects / what to watch.** One-pass rename across ~10 files; no behavior change unless some impl overrides an alias inconsistently (none found).

**Value of simplifying.** Removes a latent inconsistency trap in a trait with 12+ implementations and makes grep/graph navigation unambiguous for embedding dimensions and model identity.

**Adversarial verifier: ✅ CONFIRMED.** The embedding-provider alias pairs existed, but most `.dimension()` hits were `VectorStore::dimension`; only one real embedding-provider `dimension()` use was found. The implemented cut removed the aliases, renamed `model_name()` users in memory/knowledge/eval/loadtest/brain, and avoided unrelated vector-store APIs.

**Implementation status: ✅ DONE.** Current `EmbeddingProvider` only exposes `model_id()`, `dimensions()`, `model_version()`, and `embed()`. The remaining `dimension()` search hits are vector-store APIs or local vector-store stand-ins, and the remaining `model_name()` helper is lineage citation code unrelated to embedding providers. Verification passed with `cargo check -p moa-core -p moa-memory-ingest -p moa-memory-lifecycle -p moa-knowledge -p moa-eval -p moa-loadtest -p moa-orchestrator --locked` and `cargo check -p moa-core -p moa-providers -p moa-memory-ingest -p moa-memory-lifecycle -p moa-knowledge -p moa-eval -p moa-loadtest -p moa-orchestrator -p moa-brain --all-targets --locked`.

---

### 66. LineageHandle JSON bridge forces serialize -> deserialize -> clone -> deserialize on every lineage event

**Area:** lineage / ocsf / observability
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~150 deleted plus conversions removed at every call site**

**Locations**

- `crates/moa-core/src/traits/mod.rs:666-706 (LineageHandle trait + NullLineageHandle)`
- `crates/moa-lineage/sink/src/mpsc_sink.rs:249-352 (JSON->typed re-decode, emit_lineage_span_attributes clones the whole Value and decodes again)`
- `crates/moa-brain/src/lineage.rs:40-60,190-200,328-335`
- `crates/moa-brain/src/pipeline/memory/lineage.rs:80-95`

**What it is.** moa-core owns a 'transport-neutral' `LineageHandle` trait whose methods take `serde_json::Value`. Producers (moa-brain, orchestrator, hands ToolContext) build typed `LineageEvent`s, serialize them to JSON, and pass them through the handle; `MpscSink` then deserializes back to `LineageEvent`, and `record_span_attributes` clones the full Value and deserializes it a second time to dispatch to moa-lineage-otel. Malformed-event counters and warn paths exist solely to handle decode failures that typed calls could never produce.

**Why it may be over-engineered.** The indirection exists only so moa-core call sites avoid a dependency edge on lineage crates — but `moa-lineage-core` is a small leaf crate that itself depends only on moa-core (plus moa-memory-types), and every real producer (moa-brain, moa-orchestrator) already depends on the lineage stack via moa-lineage-citation/sink. The JSON encoding adds no capability: it is A-calls-B-calls-C where the JSON layer contributes conversions, a clone, and two impossible error paths per event on the hot path.

**Simpler alternative.** Make the handle typed: extend `moa_lineage_core::LineageSink` with the `record_durable`/`record_span_attributes`/`dropped_count` surface (or add a small typed handle trait in moa-lineage-core), have moa-brain/moa-hands/moa-orchestrator take `Arc<dyn LineageSink>` directly, and delete `LineageHandle`/`NullLineageHandle` from moa-core. `emit_lineage_span_attributes` then matches on `&LineageEvent` with no clone/decode, and `moa_lineage_malformed_total` and both decode-warn paths disappear. Pre-prod: no wire/compat concern.

**Side effects / what to watch.** Touches ~10 files across moa-core, moa-brain, moa-hands, moa-orchestrator, moa-eval (EvalLineageHandle) and one brain test double; docs/01-architecture-overview.md's trait map row for LineageHandle must be updated; moa-hands gains a direct moa-lineage-core dependency.

**Value of simplifying.** Deletes triple JSON conversion + a full-Value clone per event on the hottest observability path, removes two dead error paths and a metrics counter, and restores compile-time checking of lineage payload shapes.

**Adversarial verifier: 🟡 ADJUSTED.** The JSON bridge is real, but deleting `LineageHandle` from `moa-core` is not a simple sink swap because `ToolContext` and `BuiltInTool` carry it (`moa-core/src/traits/mod.rs:720`, `moa-hands/src/core/dispatch.rs:294`), and `moa-lineage-core` currently depends on `moa-core`. Resolve dependency direction or relocate the tool-context seam before making the bridge typed.

---

### 67. Single-variant ActionRuleScope enum threaded through scoring, experiments, and agents

**Area:** auth / agents / contacts / scoring / experiments
effort: **large** · finder confidence: **high** · ~LOC removable: **~400**

**Locations**

- `crates/moa-core/src/types/action_policy.rs (lines 117-136)`
- `crates/moa-scoring/src/lib.rs (lines 656-672, and 'scope = tenant AND user_id IS NULL' SQL at 58-112)`
- `crates/moa-experiments/src/store.rs (lines 687-703, 1054-1073)`
- `crates/moa-experiments/src/app.rs (lines 611-615)`
- `crates/moa-agents/src/resolver.rs (lines 317-324)`
- `crates/moa-artifacts/src/registry.rs (lines 27-53)`

**What it is.** ActionRuleScope is an enum with exactly one variant, Tenant { tenant_id }. It is threaded as &ActionRuleScope through ~67 files (109 ActionRuleScope::Tenant sites, ~29 single-arm `match scope` blocks). Three separate private adapters convert it to the same constant column triple ('tenant', Some(partition_id), None): ScopeParts in moa-scoring, ScopeParts in moa-experiments, and ArtifactScopeParts in moa-artifacts. moa-experiments store.rs additionally re-parses the columns back into the enum (scope_from_parts) and derives RlsContext from it (experiment_scope_context); every query binds three parameters where two are compile-time constants ('tenant', NULL).

**Why it may be over-engineered.** Speculative extensibility for a user/personal scope tier that no code, config, test, or doc ever creates: no doc mentions user-scoped rules, all writes insert user_id=NULL, all reads filter user_id IS NULL. The Postgres `scope` column is a GENERATED column (compute_scope_tier) used by RLS, so the Rust enum adds nothing the database does not already derive. Every new store function pays the match + adapter + three-bind tax for a value that is always the same.

**Simpler alternative.** Replace the ActionRuleScope parameter with a plain TenantId (or StoragePartitionId) everywhere. Delete the enum, the three ScopeParts adapters, scope_from_parts, experiment_scope_context, scoped_conn_for_artifact_scope, and tenant_scope(); bind storage_partition_id directly and drop the constant 'tenant'/NULL binds (the generated DB scope column and RLS policies stay untouched). If a user scope tier ever ships, reintroduce an enum at that point.

**Side effects / what to watch.** Wide mechanical refactor across moa-scoring, moa-experiments, moa-agents, moa-artifacts, moa-skills, and orchestrator services; serialized records embedding `scope: ActionRuleScope` (e.g. ExperimentRunRecord) change shape — acceptable pre-production. No DB migration needed since scope columns are generated.

**Value of simplifying.** Removes a whole speculative abstraction layer: ~29 match sites, 3 duplicate adapters, dozens of dead SQL binds, and simpler function signatures in every store; future store code cannot get the scope columns wrong.

**Adversarial verifier: 🟡 ADJUSTED.** The single-variant enum and adapter sprawl are real, but `action_policy_rules.scope` is explicit, constrained to tenant, and tied to tenant RLS. Simplify Rust to tenant ids/partition ids while keeping DB/RLS columns and including wire/action-policy storage side effects.

---

### 68. fga-bootstrap ships a second hand-rolled OpenFGA HTTP client that its own doc comment calls a stopgap

**Area:** auth / agents / contacts / scoring / experiments
effort: **medium** · finder confidence: **high** · ~LOC removable: **~250**

**Locations**

- `crates/moa-auth/fga-bootstrap/src/http.rs (356 lines)`
- `crates/moa-auth/authz/src/client.rs (production FgaClient)`
- `crates/moa-auth/fga-bootstrap/Cargo.toml`

**What it is.** fga-bootstrap defines its own FgaClient over reqwest implementing check, batch_check, list_objects, and tuple write/delete — all of which the production moa-authz FgaClient also implements. The module doc says: 'The production client wrapper lands in moa-authz in P1.2. This module is intentionally small and explicit so bootstrap does not depend on later authz crate work.' That production client now exists. The bootstrap copy also carries speculative dual-shape BatchCheck response parsing (map AND array forms, lines 286-355) that the production client does not need.

**Why it may be over-engineered.** Near-duplicate parallel code path whose stated justification (moa-authz did not exist yet) has expired. Two implementations of the same wire protocol must now be kept in sync, and the bootstrap one has extra parsing branches for response shapes the production client proves are unnecessary.

**Simpler alternative.** Add moa-authz as a dependency of moa-fga-bootstrap (no cycle: moa-authz does not depend on it). Keep only the three genuinely bootstrap-specific calls — find_store_by_name, create_store, write_authorization_model — as ~80 lines of local helpers (or add them to moa-authz), and route check/batch_check/list_objects/write through the production FgaClient constructed after the store/model IDs are known.

**Side effects / what to watch.** moa-fga-bootstrap gains a dependency on moa-authz (pulls moa-authz-schema which it already uses). smoke-check code in main.rs needs small call-site updates; bootstrap_live test still covers the flow.

**Value of simplifying.** Deletes ~250 lines and eliminates a drift-prone duplicate of the security-critical OpenFGA wire protocol; one client to fix when the API changes.

**Adversarial verifier: 🟡 ADJUSTED.** Bootstrap has its own stopgap `FgaClient`, duplicating post-bootstrap tuple operations, but still needs local store/model creation helpers before a production `FgaConfig` exists. Share the check/list/batch/write tuple paths with `moa-authz` where practical, but keep minimal bootstrap-only store/model helpers.

---

### 69. Six experiment score row types are field-identical mirrors of moa-scoring types, with six hand-written mapping functions

**Area:** auth / agents / contacts / scoring / experiments
effort: **small** · finder confidence: **high** · ~LOC removable: **~200**

**Locations**

- `crates/moa-core/src/wire/experiments.rs (lines 313-360 area: ExperimentScoreSummaryRow, ExperimentTrialScoreSummary, ExperimentScenarioScoreSummary, ExperimentCompareRow, ExperimentScenarioScoreDeltaRow, ExperimentVariantScoreDeltaRow)`
- `crates/moa-scoring/src/lib.rs (lines 213-356: ScoreSummaryRow, TrialScoreSummary, ScenarioScoreSummary, ScoreCompareRow, ScenarioScoreDeltaRow, VariantScoreDeltaRow)`
- `crates/moa-experiments/src/app.rs (lines 1117-1181 mapping functions)`

**What it is.** moa-core's wire module defines Experiment* score/summary/delta structs that are copy-for-copy identical (same fields, same doc comments, both serde) to the structs moa-scoring returns. moa-experiments/app.rs exists largely to convert one set into the other via six trivial field-by-field mapping functions (experiment_score_summary_row, experiment_trial_score_summary, experiment_scenario_score_summary, experiment_compare_row, experiment_scenario_score_delta_row, experiment_variant_score_delta_row) plus per-response .into_iter().map(...) plumbing.

**Why it may be over-engineered.** A parallel type layer that adds no invariant, renaming, or shape difference — pure duplication between a crate and its own dependency's public types. moa-scoring already depends on moa-core, so a single shared definition is structurally possible today. With no external users there is no wire-stability reason to keep an independent copy.

**Simpler alternative.** Keep one set of row types in moa-core (wire module) and have moa-scoring construct and return those types directly; delete the moa-scoring duplicates and all six mapping functions, letting Experiment responses embed the rows as-is.

**Side effects / what to watch.** moa-scoring's public API changes to return moa-core wire types; scoring/experiments tests referencing the old type paths need import updates. No behavior change.

**Value of simplifying.** Deletes ~200 lines of mirror structs and copy code; adding a score column becomes a one-place change instead of three (scoring type, wire type, mapper).

**Adversarial verifier: 🟡 ADJUSTED.** Experiment wire score rows mirror `moa-scoring` rows and are mapped field-by-field, but eval has the same mirror pattern. Prefer neutral shared score DTOs in `moa-core` reused or wrapped by experiment/eval responses; do not make `moa-scoring` return experiment-prefixed DTOs.

---

### 71. API-key validation uses two global caches plus per-entry mutexed revocation-recheck timestamps where one short-TTL cache gives the same guarantees

**Area:** auth / agents / contacts / scoring / experiments
effort: **small** · finder confidence: **medium** · ~LOC removable: **~110**

**Locations**

- `crates/moa-auth/providers/src/api_keys.rs (lines 28-59, 338-402, 488-526)`

**What it is.** validate() maintains a forward moka cache (blake3(key) -> CachedValidation, TTL 60s), a second reverse-index moka cache (key_id -> cache_key) solely so revoke() can invalidate in-process, and each cached entry carries an Arc<Mutex<Instant>> so that a cache hit older than 5s (REVOCATION_RECHECK_INTERVAL) runs a DB EXISTS re-check and advances the timestamp in place.

**Why it may be over-engineered.** The layered design's only benefit over a plain 5-second-TTL cache is (a) instant in-process revocation and (b) replacing one argon2 verify per key per 5s with a cheaper EXISTS query. But out-of-process revocation is already accepted to take up to 5s (the comment says so), so the global revocation guarantee is 5s either way, and argon2 once per key per 5s is negligible CPU. Three moving parts (second cache, mutexed timestamps, recheck branch) buy nothing measurable; no perf doc justifies it.

**Simpler alternative.** One moka cache with time_to_live = 5s storing ResolvedKey. Delete CachedValidation, VALIDATION_KEY_IDS, recheck_due, cached_key_is_active, validate_cached_resolution, and invalidate_validation_cache_for_key_id; revoke() no longer touches caches. Revocation takes effect within 5s everywhere, exactly the current cross-process bound.

**Side effects / what to watch.** In-process revocation latency goes from ~0s to <=5s (already the cross-instance bound, and MOA is pre-production); each cached key costs one argon2 verify per 5s instead of one EXISTS query per 5s. Three cache-behavior unit tests are deleted or simplified.

**Value of simplifying.** Removes a second global cache, per-entry locking, and a subtle consistency protocol from the hot authentication path — fewer failure points in security-critical code.

**Adversarial verifier: 🟡 ADJUSTED.** The two-cache plus per-entry revocation-recheck stack exists, but a 5s TTL cache miss would rerun prefix lookup, Argon2 verification, and `last_used_at` update attempts, not only Argon2. A one-cache design is viable only with the latency/cost tradeoff and loss of same-process instant revocation stated explicitly.

---

### 75. MemoryOverride config knobs are pure speculative surface: two knobs hard-error 'not implemented' and the third is a no-op with a dead helper

**Area:** eval crates
effort: **small** · finder confidence: **high** · ~LOC removable: **~100**

**Locations**

- `crates/moa-eval/core/src/types.rs:267-277`
- `crates/moa-eval/src/setup.rs:184-203`
- `crates/moa-eval/src/setup.rs:415-423`
- `crates/moa-eval/src/setup.rs:563-584`
- `crates/moa-eval/examples/example-config-baseline.toml:10-12`
- `crates/moa-eval/examples/example-config-variant.toml:10-13`

**What it is.** AgentConfig carries a `MemoryOverride { tenant_memory_path, user_memory_path, clear_defaults }` TOML section. `seed_memory()` in setup.rs returns an InvalidConfig error ("...seeding is not implemented") when either path is set, and for `clear_defaults=false` calls `configured_default_memory_root()` — which resolves cloud/local memory dirs and expands `~` — only to discard the result via `let _ = default_root;`. A dedicated test pins that setting the knob fails, and both example config TOMLs advertise the knobs.

**Why it may be over-engineered.** This is config surface for a feature that does not exist. Any user who sets the documented example values gets a hard error at environment build time; `clear_defaults` silently does nothing either way; and `configured_default_memory_root` is dead computation kept alive only by the no-op branch. In a pre-production repo with no compatibility requirement, a TOML schema reserved 'for later' plus a test pinning its own unimplementedness is pure carrying cost.

**Simpler alternative.** Delete `MemoryOverride` from AgentConfig/AgentConfigBody, delete `seed_memory`, `configured_default_memory_root`, and the `setup_rejects_unimplemented_memory_seed_paths` test, and remove the `[agent.memory]` blocks from both example TOMLs. When memory fixture seeding is actually implemented, add the fields back together with the implementation.

**Side effects / what to watch.** TOML configs containing `[agent.memory]` would be silently ignored instead of erroring (serde default, no deny_unknown_fields) — acceptable since the only such configs are the crate's own examples, which get fixed in the same change. `expand_local_path` stays because `resolve_path`/workspace_instructions still use it.

**Value of simplifying.** ~100 lines deleted, one misleading example config fixed, and the AgentConfig schema stops promising a capability that instantly errors.

**Adversarial verifier: 🟡 ADJUSTED.** The hard-error and no-op memory override knobs are real, but the shape is also part of JSON reporter output and eval-core re-exports. Remove it, but update/delete the reporter path too if the reporter subsystem is not deleted first.

---

### 76. Dead API surface in moa-eval-core and EvalEngine: unused discovery helpers, never-constructed error variants, an uncalled engine method, and an errors-only enum variant

**Area:** eval crates
effort: **small** · finder confidence: **high** · ~LOC removable: **~90**

**Locations**

- `crates/moa-eval/core/src/loader.rs:36-74`
- `crates/moa-eval/core/src/error.rs:35-37`
- `crates/moa-eval/core/src/error.rs:56-61`
- `crates/moa-eval/src/engine.rs:84-98`
- `crates/moa-eval/core/src/types.rs:99-105`
- `crates/moa-eval/core/src/types.rs:149-165`

**What it is.** Several public items exist with no consumer: (a) `discover_suites`/`discover_configs`/`discover_matching_toml_files`/`discover_toml_files` in loader.rs are called only by tests/eval_offline/loader.rs — the orchestrator Eval service parses suite documents from Postgres, not directories; (b) `EvalError::ApprovalRequired` and `EvalError::SerializeToml` are never constructed anywhere in the workspace; (c) `EvalEngine::run_single_with_provider` has zero callers; (d) `LongConversationMode::Live` exists only so `long_case()` can reject it with "live mode, which is not implemented", and `LongConversationMode::is_recorded` is a helper for that speculative distinction.

**Why it may be over-engineered.** Each item is capacity reserved for a future that has not arrived: filesystem suite discovery for a CLI that doesn't exist, an approval-blocking error path never raised, a provider-injection variant of run_single nobody uses, and a third conversation mode whose entire behavior is an error message. None is load-bearing; all widen the public API that every consumer (orchestrator, skills) must compile and readers must understand.

**Simpler alternative.** Delete the four discovery functions and their test, the two error variants, `run_single_with_provider`, and the `Live` enum variant (unknown TOML mode values then fail at serde parse time with a clearer error than the hand-written one). Fold what remains of the loader test into the load_suite/load_agent_config assertions.

**Side effects / what to watch.** Serde parse errors replace the bespoke 'live mode not implemented' message — arguably better. No runtime behavior changes since nothing reaches these paths.

**Value of simplifying.** ~90 lines deleted and a visibly smaller eval-core API; removes three separate 'why does this exist?' traps for readers.

**Adversarial verifier: 🟡 ADJUSTED.** Discovery helpers, unused error variants, and `run_single_with_provider` are unused outside tests/public surface. Correction: `LongConversationMode::Live` is rejected in both `long_case()` and transcript execution, so cleanup must update both branches.

**Implementation status: ✅ DONE.** Deleted the discovery helper API and its discovery-only test, removed `EvalError::SerializeToml` / `ApprovalRequired`, removed `EvalEngine::run_single_with_provider`, and removed `LongConversationMode::Live` plus both rejection branches. Verification passed with `cargo test -p moa-eval --test eval_offline --locked loader` and `cargo check -p moa-eval-core -p moa-eval --all-targets --locked`.

---

### 79. check-architecture-boundaries: 99-entry counted-allowance ledger plus numeric LOC/symbol/package-count ratchets

**Area:** loadtest / test-support / xtask / scripts
effort: **large** · finder confidence: **medium** · ~LOC removable: **~500**

**Locations**

- `crates/xtask/src/check_architecture_boundaries.rs:60-768`
- `crates/xtask/src/check_architecture_boundaries.rs:770-816`
- `crates/xtask/src/check_architecture_boundaries.rs:1619-1680`

**What it is.** A 2,433-line bespoke lint engine run in CI. Beyond a few mechanical rules (direct SQL in handlers, handler-authz safety, forbidden dependency directions, wildcard Event matches), it maintains a central ALLOWANCES table of 99 macro-built entries, each pinning an exact (file, needle, expected_count) triple, plus exact-value ratchet budgets: per-file/per-tree LOC budgets (e.g. moa-core src max_lines 21,096; turn_execution.rs max_lines 2,969), a pub-use symbol budget (87), workspace package-count budgets (43/40), and reverse-dependency budgets (37/38).

**Why it may be over-engineered.** The counted-allowance mechanism makes every refactor that adds, removes, or moves one occurrence of a needle string a two-file change (code + ledger), and the exact-count LOC/symbol/package budgets fail CI on any net growth of hot files until someone bumps a magic number whose value (21_096) carries no meaning beyond 'what the file happened to be at last bump'. Roughly 700 lines of the file are the ledger itself. The high-signal rules (authz safety, direct SQL, dependency direction) do not need this machinery; the numeric budgets largely duplicate what code review and this very audit process do, at permanent table-churn cost.

**Simpler alternative.** Keep the mechanical rules (DirectSql, HandlerAuthzSafety, ForbiddenDependency, EventWildcardMatch, RuntimeContext detection). Replace the central counted ledger with an inline, self-locating marker convention — a `// BOUNDARY-ALLOW: <reason>` comment on the offending line that the checker accepts — so exceptions move with the code and no counts are maintained. Delete the LOC/symbol/workspace-count/reverse-dependency budget rules outright, or reduce them to a single warn-only report.

**Side effects / what to watch.** Loses the hard ratchet that stops silent growth of moa-core/turn_execution/routes.rs; the team clearly built this deliberately as an engineering-discipline tool, so removing budgets is a policy decision, not just a code cleanup. Inline markers are slightly easier to add without review noticing than a central table diff.

**Value of simplifying.** Removes ~500+ lines of table/engine code and the recurring CI-failure/table-bump loop on every ordinary refactor of the budgeted files, while keeping all mechanical safety rules.

**Adversarial verifier: 🟡 ADJUSTED.** The architecture-boundary checker has counted allowances and hard budgets, but it is load-bearing policy, run in CI and documented. Replace counted allowlists with inline markers or warn-only budgets only if the team accepts weaker governance.

---

### 80. Generic SessionStore 'contract tests' in moa-test-support with exactly one implementation and one consumer

**Area:** loadtest / test-support / xtask / scripts
effort: **medium** · finder confidence: **high** · ~LOC removable: **~450 relocated, ~60 net deleted**

**Locations**

- `crates/moa-test-support/src/postgres/contracts/session.rs`
- `crates/moa-test-support/src/postgres/contracts/action_policy.rs`
- `crates/moa-test-support/src/postgres/contracts/mod.rs`
- `crates/moa-session/tests/postgres_store_db.rs:15-17,131-135`

**What it is.** moa-test-support exports seven generic test functions (test_create_and_get_session<S: SessionStore + ?Sized>, test_emit_and_get_events, test_event_search, test_list_sessions_with_filter, test_session_status_update, test_tenant_cost_since, test_action_policy_rules) as 'shared trait-level contract tests for Postgres-backed stores' (~440 lines across contracts/). Grep confirms they are called from exactly one file, crates/moa-session/tests/postgres_store_db.rs, against exactly one production implementation, PostgresSessionStore (all other SessionStore impls in the workspace are per-crate test mocks that never run these contracts).

**Why it may be over-engineered.** The generic type parameter, the cross-crate module, and the re-export ladder exist to serve a hypothetical second store backend that has no concrete plan. A trait 'contract suite' is only worth its indirection when at least two implementations run it; with one impl and one caller it is just ordinary test code living two crates away from the store it tests, behind a needless <S: SessionStore + ?Sized> bound.

**Simpler alternative.** Move the test bodies into crates/moa-session/tests/postgres_store_db.rs (the per-lane harness that already calls them), monomorphized to &PostgresSessionStore, and delete moa-test-support/src/postgres/contracts entirely along with its pub use re-exports. If a second store ever appears, re-extract then — pre-prod, nothing depends on the current shape.

**Side effects / what to watch.** postgres_store_db.rs grows by ~400 lines (net workspace LOC roughly unchanged); moa-test-support's postgres module shrinks to the TestDb bootstrap helpers. No coverage is lost since no other crate invokes the contracts.

**Value of simplifying.** Removes a cross-crate indirection layer and generic machinery with a single concrete instantiation; tests live next to the store they pin, and moa-test-support's API surface shrinks.

**Adversarial verifier: 🟡 ADJUSTED.** The exported contract helpers are effectively single-consumer, but `test_action_policy_rules` targets `ActionPolicyRuleStore`, not `SessionStore`, and a test-only `StaticRuleStore` exists. Relocating these tests into the moa-session DB harness still holds with that correction.

---

### 81. perf-gate scenario layer duplicates report/gate plumbing inside moa-loadtest, including a dead public export

**Area:** loadtest / test-support / xtask / scripts
effort: **medium** · finder confidence: **high** · ~LOC removable: **~150**

**Locations**

- `crates/moa-loadtest/src/scenarios/mock_smoke.rs:106-126,207-316`
- `crates/moa-loadtest/src/scenarios/retrieval/reporting.rs:116-148,206-222`
- `crates/moa-loadtest/src/scenarios/retrieval/mod.rs:46`
- `crates/moa-loadtest/src/metrics.rs:26-57`
- `crates/moa-loadtest/src/scenarios/retrieval/load.rs:303-309`

**What it is.** The two perf_gate profiles each carry their own copy of the same plumbing: write_snapshot, write_stdout, write_stderr, and sanitize_prom_comment are copy-pasted verbatim between mock_smoke.rs and retrieval/reporting.rs; mock_smoke's validate_config re-validates duration/rate that LoadTestOptions::validate already checks; and the crate now contains four independent percentile implementations (HdrHistogram in hist.rs, nearest-rank percentile() in metrics.rs, ceil-rank percentile_sorted() in load.rs, and two Prometheus-bucket variants in metrics.rs/reporting.rs). reporting.rs also exports `pub fn histogram_percentile` (re-exported at retrieval/mod.rs:46) that has zero consumers outside its own unit test.

**Why it may be over-engineered.** This is parallel near-duplicate code inside a single crate: the helpers were copied rather than shared when the mock-short profile was added, and the dead histogram_percentile export is speculative public API. Each copy is a place for behavior to drift (the two percentile rank conventions already disagree: round vs ceil).

**Simpler alternative.** Create one small scenarios/report_util module holding write_snapshot/write_stdout/write_stderr/sanitize_prom_comment and a single sample-percentile function; delete the dead `pub use reporting::histogram_percentile` and the function; drop mock_smoke's redundant validate_config lines that duplicate LoadTestOptions::validate. Optionally go further and fold the mock-short profile into the moa-loadtest binary as --max-p95-ms/--max-error-rate/--prom-out flags, deleting most of mock_smoke.rs.

**Side effects / what to watch.** Purely internal to moa-loadtest; CI invocations of `perf_gate --profile mock-short` keep working (or, with the binary-fold option, deploy.yml/perf-gate.yml commands change to moa-loadtest flags). Percentile unification may shift reported values by one rank convention.

**Value of simplifying.** Deletes ~150 lines of duplicated helpers and a dead export, and leaves one percentile definition for client-side sample math instead of three.

**Adversarial verifier: 🟡 ADJUSTED.** Duplicate reporting helpers and a dead public percentile export exist, but `mock_smoke::validate_config` has unique max-error-rate and virtual-user-cap checks. Share helper plumbing and remove the dead export, while keeping unique validation.

---

### 84. Session-store 'focused contract' trait family: 8 single-impl traits + supertrait all backed by one PostgresSessionStore

**Area:** cross-cutting: single-impl abstractions
effort: **medium** · finder confidence: **high** · ~LOC removable: **~350**

**Locations**

- `crates/moa-core/src/traits/mod.rs:188-471`
- `crates/moa-core/src/traits/mod.rs:517-554`
- `crates/moa-orchestrator/src/ctx.rs:26-133`
- `crates/moa-orchestrator/src/ctx.rs:392-500`
- `crates/moa-session/src/store/ (impl blocks, e.g. session_store.rs:898, session_attachments.rs:10)`
- `crates/moa-eval/src/setup.rs:35-129`
- `crates/moa-brain/src/pipeline/builder.rs:93`
- `crates/moa-brain/src/pipeline/skills/mod.rs:44-120`

**What it is.** moa-core defines SessionStore plus 7 'focused contract' sub-traits (SessionChannelStore, SessionEventLookupStore, SessionAnalyticsStore, SessionLearningLogStore, SegmentStore, ExperienceStore, LearningCandidateStore), a SessionRepository supertrait that unions all 8, a blanket `impl<T> SessionRepository for T`, and a separate SessionAttachmentStore. The orchestrator's PersistenceDeps holds 10 separate Arc<dyn ...> fields plus one Arc<PostgresSessionStore>, all created by cloning the same Arc 11 times, with 11 accessor methods; OrchestratorCtx re-forwards several of them. moa-eval/setup.rs repeats the same multi-clone dance.

**Why it may be over-engineered.** Every sub-trait and SessionAttachmentStore has exactly ONE implementation in the whole workspace — PostgresSessionStore — and zero test doubles (grep 'impl <Trait> for' returns 1 hit each, including tests). The interface segregation buys nothing: no consumer is ever handed a different backend, no test narrows to a sub-trait (test mocks implement the big SessionStore trait, and moa-edge already holds the concrete Arc<PostgresSessionStore>). The result is 9 trait definitions, a blanket impl, 8 parallel impl blocks in moa-session, and an 11-field dependency struct that all describe one object.

**Simpler alternative.** Merge the 7 sub-traits' and SessionAttachmentStore's methods into the single SessionStore trait (using the existing default-`Err(MoaError::Unsupported)` pattern so MockSessionStore/RecordingSessionStore test doubles compile unchanged). Delete SessionRepository, its blanket impl, and the 7+1 trait definitions. Collapse PersistenceDeps to two fields: `session_store: Arc<dyn SessionStore>` and `session_store_backend: Arc<PostgresSessionStore>` (plus graph_pool). Collapse the 8 impl blocks in moa-session into one. Update ~30 call sites mechanically from `deps.segment_store()` etc. to `deps.session_store()`.

**Side effects / what to watch.** Mechanical rename churn across moa-orchestrator, moa-brain, moa-eval call sites; SessionStore trait becomes large (but it already is, and defaults keep doubles small); docs/01 trait table row for SessionStore absorbs the sub-contract mention. No wire, DB, or behavior change; existing _db/_offline tests should pass unmodified after signature updates.

**Value of simplifying.** Deletes ~8 abstraction seams and roughly 350 net lines; the orchestrator dependency root drops from 11 store fields to 2, making 'where does session data go' answerable with one type instead of nine.

**Adversarial verifier: 🟡 ADJUSTED.** The single-impl trait-family claim holds, but the proposal needs a wider accounting: `PersistenceDeps` also includes `session_repository` and `action_policy_store`, and `SessionAttachmentStore` is used by edge upload routes. Include or explicitly exclude `ActionPolicyRuleStore`, and treat attachment merging as an edge-surface change.

---

### 87. BranchManager trait with one impl, no dyn usage, and consumers that already name the concrete type

**Area:** cross-cutting: single-impl abstractions
effort: **small** · finder confidence: **high** · ~LOC removable: **~60**

**Locations**

- `crates/moa-core/src/traits/mod.rs:556-580`
- `crates/moa-session/src/neon.rs`
- `crates/moa-orchestrator/src/services/neon_maint.rs:6-63`
- `crates/moa-orchestrator/src/services/admin_maintenance.rs:12-275`

**What it is.** moa-core defines the BranchManager trait (create_checkpoint/rollback_to/discard_checkpoint/list_checkpoints/cleanup_expired). The only implementation is NeonBranchManager in moa-session. Both consumers construct `moa_session::NeonBranchManager::from_config(...)` concretely and import the trait solely so the method calls resolve. A code comment on the trait itself notes it is deliberately not dyn-compatible because no `dyn BranchManager` exists in the workspace.

**Why it may be over-engineered.** The trait provides no polymorphism (no dyn, no generic bounds anywhere), no test seam (1 impl total including tests), and no decoupling (consumers already depend on moa-session and name NeonBranchManager). It exists only as ceremony between moa-core and one concrete struct; the docs already describe checkpoints as a Neon-specific optional feature, so a second backend is not planned.

**Simpler alternative.** Delete the trait from moa-core and make the five methods inherent on NeonBranchManager; drop the `use moa_core::BranchManager` imports. Optionally move CheckpointHandle/CheckpointInfo DTOs to moa-session with it.

**Side effects / what to watch.** docs/01 trait table loses one row; moa-core public API shrinks (pre-prod, no compat concern). Live Neon test (`neon_branch_manager_live.rs`) needs its import updated.

**Value of simplifying.** One less core trait to document and keep in sync; checkpointing becomes an ordinary module in the crate that owns it.

**Adversarial verifier: 🟡 ADJUSTED.** `BranchManager` is one-impl and consumers call concrete `NeonBranchManager`, but do not broaden this into deleting Neon or moving checkpoint wire types; edge exposes checkpoint routes and cron wires `NeonMaint`. Narrow simplification: make methods inherent if desired, while keeping the operational surface and wire DTOs.

---

### 89. experiments and internal-eval-runner features are enabled by no build, CI job, or script — ~10k lines of never-compiled subsystem

**Area:** cross-cutting: config & feature-flag sprawl
effort: **large** · finder confidence: **high** · ~LOC removable: **~10400**

**Locations**

- `crates/moa-orchestrator/Cargo.toml:13,17-24,33-63`
- `crates/moa-experiments/src (4,358 src lines incl. app.rs 1,408)`
- `crates/moa-scoring/src`
- `crates/moa-orchestrator/src/services/experiments.rs (1,052 lines)`
- `crates/moa-orchestrator/src/services/eval/ (1,971 lines)`
- `crates/moa-orchestrator/src/workflows/mod.rs:8-17`
- `crates/moa-orchestrator/src/runtime/endpoint.rs:13-17,133-144`

**What it is.** The orchestrator declares `experiments` (pulls in moa-experiments + moa-scoring, gates services/experiments.rs and three experiment workflows) and `internal-eval-runner` (gates services/eval/ and much of skill_regression.rs) features. Seven test targets carry `required-features` for them (Cargo.toml:33-63).

**Why it may be over-engineered.** Nothing enables either flag: Dockerfile builds with `redis`, docker-compose with `redis,provider-overrides`, run-clean-e2e.sh with `provider-overrides,skill-learning,redis[,integration]`, CI runs `cargo nextest run --profile ci` and `cargo clippy --all-targets` with default features, k8s/fly set nothing. The 7 required-features test targets never run, and the gated orchestrator code is never even type-checked in CI, so it silently rots. moa-experiments and moa-scoring (~6,250 lines with tests) exist solely to feed these dead flags.

**Simpler alternative.** Pick one: (a) delete moa-experiments, moa-scoring, services/experiments.rs, services/eval/, the experiment workflows, both features, and the 7 test targets until a real consumer exists (pre-prod, no users); or (b) if the behavior-lab is wanted now, remove the two feature flags, compile the code unconditionally, and let the 7 test targets run in normal CI. Either option removes the cfg forest in endpoint.rs/workflows/mod.rs and the never-executed test matrix.

**Side effects / what to watch.** Option (a) deletes the experiment/behavior-lab capability described in docs/product/behavior-lab.md and docs/09; those docs would need updating. Option (b) grows the production binary and exposes the internal Eval/experiment Restate services in prod deployments, which docs currently say should stay internal-only — would need an env-level enable check instead of a compile-time gate.

**Value of simplifying.** Removes ~10,400 lines of un-CI'd dead code (or brings it under CI), two workspace crates, two feature flags, and 7 never-run test binaries that slow nextest listing.

**Adversarial verifier: 🟡 ADJUSTED.** Orchestrator experiment/eval-runner integrations are not enabled by repo build paths, but `moa-experiments` and `moa-scoring` are default workspace members and edge experiment route translations are unconditional. Deletion/guarding must include edge routes and wire docs, not just orchestrator features.

---

### 90. 150 of ~245 MOA_* env knobs are write-only: supported by MoaEnvOverlay but never set anywhere in the repo

**Area:** cross-cutting: config & feature-flag sprawl
effort: **medium** · finder confidence: **high** · ~LOC removable: **~600**

**Locations**

- `crates/moa-core/src/config/env_overlay.rs:22-585 (overlay fields) and apply_to at 586-700`
- `crates/moa-core/src/config/context.rs (SessionLimitsConfig, CompactionConfig, ToolBudgetConfig, ResolutionConfig/Weights, QueryRewriteConfig)`
- `crates/moa-core/src/config/providers.rs:74-91 (per-provider pacing Options)`
- `crates/moa-core/src/config/memory.rs`
- `crates/moa-core/src/config/knowledge.rs`

**What it is.** MoaConfig loads exclusively from flat MOA_* env vars via the ~245-field MoaEnvOverlay (envy-derived Options plus per-field apply_to plumbing and overlay unit tests). I cross-referenced every supported env name against .env.example, docker-compose*.yml, k8s/, Makefile, scripts/, docs/, live/, and all .rs files outside crates/moa-core/src/config.

**Why it may be over-engineered.** 150 knobs are referenced nowhere outside the config module itself — e.g. all 14 MOA_SESSION_LIMITS_*, all 8 MOA_COMPACTION_*, all 9 MOA_RESOLUTION_* weights/thresholds, all 8 MOA_TOOL_BUDGETS_*, all 8 MOA_QUERY_REWRITE_* (incl. 3 circuit-breaker knobs), all 15 provider pacing knobs (*_MAX_REQUESTS_PER_MIN/_MAX_INPUTS_PER_MIN/_MAX_CONCURRENT_REQUESTS — which default to None = pacer disabled, so LLM-side RatePacer is inert in every deployment, crates/moa-providers/src/adapters/anthropic/mod.rs:96,120-124), 7 MOA_AUTH_CONTACT_TOKENS_*, and 9 MOA_DATABASE_NEON_*. Each dead knob costs an overlay field + doc comment + apply line + config-struct field default + test surface, and every knob is a value someone can set to break production.

**Simpler alternative.** Trim MoaEnvOverlay to the ~95 env vars actually set somewhere (env.example, compose, k8s, docs, or integration tests). For the removed knobs keep the behavior as plain struct defaults/constants; tests that vary behavior already mutate MoaConfig struct fields directly and are unaffected. Delete the corresponding apply_to lines and overlay tests.

**Side effects / what to watch.** Loses env-level ops escape hatches: tuning e.g. session limits or compaction thresholds in an incident would require a code change + redeploy instead of an env edit. Reasonable pre-prod; individual knobs can be re-added the day someone actually needs to set one.

**Value of simplifying.** Cuts the runtime config surface by ~60%, shrinks env_overlay.rs (~1,330 lines) and the config structs by roughly a third, and makes the remaining knobs — the ones that actually matter — auditable at a glance.

**Adversarial verifier: 🟡 ADJUSTED.** The env overlay is broad, but several examples are referenced by loadtest edge config, integration tests, or docs. Do a per-group trim of stale or misnamed knobs instead of a blanket keep-only-vars-set-somewhere rule.

---

### 92. Daytona/E2B hand-provider features are enabled by nothing, making 1,700 lines of adapters and their config knobs unreachable in every artifact

**Area:** cross-cutting: config & feature-flag sprawl
effort: **medium** · finder confidence: **high** · ~LOC removable: **~1700**

**Locations**

- `crates/moa-hands/Cargo.toml:9-10`
- `crates/moa-hands/src/adapters/daytona/ and adapters/e2b/ (1,700 lines)`
- `crates/moa-hands/src/core/construction.rs:88-105,315-336`
- `crates/moa-hands/tests/daytona_live.rs:8 and tests/e2b_live.rs (#![cfg(feature = ...)])`
- `crates/moa-core/src/config/sandbox.rs:49-60 (CloudHandsConfig)`
- `crates/moa-core/src/config/env_overlay.rs (MOA_CLOUD_HANDS_* fields)`

**What it is.** moa-hands gates its Daytona and E2B cloud sandbox adapters behind `daytona`/`e2b` features. No consumer crate, no build script, no CI job, no Dockerfile/compose/k8s/fly config, and no nextest invocation passes them; even the live tests are feature-gated files that nothing can select. CloudHandsConfig plus 5+ MOA_CLOUD_HANDS_* env knobs (API URLs, default image, template, domain) exist to configure them and are never set anywhere.

**Why it may be over-engineered.** docs/10 lists Daytona/E2B as the cloud hand providers, but the cloud image itself (Dockerfile, features=`redis`) cannot contain them — the feature exists only as a manual `cargo build --features daytona` nobody scripts. This is a speculative provider integration kept behind a flag with no enablement path, plus a dead config subtree mirroring it.

**Simpler alternative.** Decide which cloud sandbox is actually planned: compile that one unconditionally (delete both feature flags, the cfg branches in construction.rs/lib.rs/adapters/mod.rs, and give the live test a normal MOA_RUN_LIVE_* + required-features-free shape), and delete the other adapter with its CloudHandsConfig fields and env knobs. If neither is imminent, delete both adapters — the local/Docker hands path is unconditional and is what every environment uses today.

**Side effects / what to watch.** Deleting an adapter loses that vendor integration until reimplemented (git history preserves it). docs/06 and docs/10 hand-provider tables need a one-line update. No runtime impact: no environment can currently construct these providers anyway.

**Value of simplifying.** Removes two never-buildable feature flags, ~1,700 lines of unreachable adapter code (or brings one adapter into the real build), dead config knobs, and live tests that cannot currently be run by any documented command.

**Adversarial verifier: 🟡 ADJUSTED.** No scripted build enables `daytona`/`e2b`, and live tests are feature-cfg'd out. Correction: `.env.example` sets `MOA_CLOUD_HANDS_DEFAULT_PROVIDER=daytona`, while `fly.toml` sets local. Remove empty compile feature gates and keep runtime config gating; delete adapters only if product direction changes.

---

### 94. Vestigial auth0 feature layer: an inner feature flag that gates nothing, on a provider chain no artifact ever ships

**Area:** cross-cutting: config & feature-flag sprawl
effort: **small** · finder confidence: **high** · ~LOC removable: **~10 (inner flag) / ~1400 (full Auth0 path)**

**Locations**

- `crates/moa-auth/auth0/Cargo.toml ([features] auth0 = [] — referenced by zero cfg attributes in its own src/ or tests/)`
- `crates/moa-auth/providers/Cargo.toml (auth0 = ["dep:moa-auth-providers-auth0"])`
- `crates/moa-orchestrator/Cargo.toml:12 and crates/moa-edge/Cargo.toml (forwarding auth0 features)`
- `.github/workflows/deploy.yml:101-103 (only place the flag is ever built)`

**What it is.** Auth0/OIDC support is gated through a three-level feature chain: edge/orchestrator `auth0` → moa-auth-providers `auth0` (optional dep) → the moa-auth-providers-auth0 crate, which itself declares an `auth0 = []` feature that no cfg attribute, test, or manifest references. The only enablement anywhere is a CI 'Build Auth0 feature targets' compile check; the deployed images (Dockerfile features=`redis`, fly/k8s defaults) never include it, and the MOA_AUTH_OIDC_* / MOA_AUTH_AUTH0_WEBHOOK_SECRET knobs are never set anywhere.

**Why it may be over-engineered.** The inner `auth0 = []` feature is pure dead weight — enabling or disabling it changes nothing since the crate has no cfg(feature="auth0") gates. The outer chain keeps 1,381 lines of provider code (auth0_provider, oidc_provider, ciba, jwks_cache, vault) compiled only by a CI check, for a pre-prod product whose every environment uses local/disabled auth. The crate boundary already provides the optionality; the extra flags add nothing.

**Simpler alternative.** Minimum: delete the no-op `auth0` feature from crates/moa-auth/auth0/Cargo.toml. Better: collapse the gating to a single switch — keep only moa-auth-providers' optional dep (`dep:moa-auth-providers-auth0`) and drop the per-binary forwarding features, or (if Auth0 is not on the near-term roadmap) delete the crate and its env knobs entirely and re-add when a real IdP integration lands.

**Side effects / what to watch.** Deleting only the inner feature: none. Dropping the whole Auth0 crate conflicts with docs/10's stated plan for Auth0/OIDC as the managed-identity path, so that variant should be a product decision; the CI compile-check step in deploy.yml would be removed either way.

**Value of simplifying.** Removes a feature flag that provably does nothing, and forces a decision on 1,381 lines of provider code whose only consumer is a CI compile check.

**Adversarial verifier: 🟡 ADJUSTED.** The inner `crates/moa-auth/auth0` feature `auth0 = []` is a no-op and should be deleted. Correction: the outer provider/edge/orchestrator feature chain is real optional dependency gating, CI compile-checks it, and docs provide Auth0 setup. Do not delete the Auth0 path without a product decision.

---

### 98. SQL identifier quoting defined 19 times and the search-path pool builder duplicated across three production binaries despite moa-db existing as the shared storage-helpers crate

**Area:** cross-cutting: cross-crate duplication
effort: **small** · finder confidence: **high** · ~LOC removable: **~120**

**Locations**

- `crates/moa-orchestrator/src/runtime/database.rs:20-58`
- `crates/moa-session/src/store/mod.rs:449-495`
- `crates/moa-edge/src/main.rs:61-68`
- `crates/moa-session/src/blob.rs:493`
- `crates/moa-session/src/analytics.rs:568`
- `crates/moa-session/src/testing.rs:490`
- `crates/moa-session/src/store/helpers.rs:107`
- `crates/moa-migrations/src/lib.rs:434`
- `crates/moa-test-support/src/fixtures.rs:19`

**What it is.** The 3-line quote_identifier function is defined 19 times across the workspace (grep 'fn quote_identifier'), including four copies inside moa-session alone and copies in moa-migrations, moa-orchestrator, and moa-test-support (whose copy is public and documented as 'centralizing logic previously copy-pasted' — yet the others remain). The 'PgPoolOptions + after_connect set_config(search_path) + schema, public' pool construction is implemented separately in moa-orchestrator/src/runtime/database.rs (build_database_pool + database_search_path) and moa-session/src/store/mod.rs (connect_with_retry, which additionally wraps it in backon retry), and moa-edge/src/main.rs builds a third ad-hoc pool from clap args without the search-path hook.

**Why it may be over-engineered.** Identifier quoting and search-path pool setup are exactly the kind of shared storage plumbing the workspace already created moa-db for (ScopedConn, GUC helpers live there). Two near-identical production pool builders means the search-path escaping and quoting rules are enforced in two places; edge's pool not applying the schema search path at all is the kind of inconsistency this duplication breeds.

**Simpler alternative.** Move quote_identifier and one pool builder (url, schema: Option<&str>, min/max, acquire timeout, optional backon connect-retry) into moa-db. moa-session, moa-orchestrator, and moa-edge call it; delete the four intra-moa-session copies in favor of the moa-db import; test files import moa_test_support::fixtures::quote_identifier (or the moa-db one) instead of redefining it. Orchestrator's database_search_path and session's connect_with_retry become thin calls.

**Side effects / what to watch.** moa-edge would start applying the configured schema search path like the other binaries — verify that is intended (it currently reads only public-schema tables via its own pool). Crates gain a moa-db dependency where missing. No migration or wire impact.

**Value of simplifying.** Removes ~100-150 lines and, more importantly, collapses connection-scoping (search_path) and identifier-escaping — both correctness/security-adjacent — from many enforcement points to one.

**Adversarial verifier: 🟡 ADJUSTED.** SQL quoting and pool setup duplication hold, but current count is 18 `quote_identifier` definitions, not 19. Edge/session/orchestrator already depend on `moa-db`, so move helpers there without new dependency churn and verify edge search-path behavior before changing it.

---

## ❌ Refuted findings (considered and rejected — do not act)

_Listed for completeness so you know these were evaluated and the complexity was judged load-bearing or the deletion premise was wrong._

### 93. Neon branching stack (~1,300 lines + 9 env knobs) is configured off in every environment

**Area:** cross-cutting: config & feature-flag sprawl
effort: **medium** · finder confidence: **high** · ~LOC removable: **~1300**

**Locations**

- `crates/moa-session/src/neon.rs (861 lines)`
- `crates/moa-orchestrator/src/services/neon_maint.rs (80 lines)`
- `crates/moa-orchestrator/src/services/admin_maintenance.rs, src/runtime/jobs.rs (wiring)`
- `crates/moa-core/src/config/database.rs:51 (DatabaseNeonConfig)`
- `crates/moa-core/src/config/env_overlay.rs (9 MOA_DATABASE_NEON_* fields)`
- `crates/moa-session/tests/neon_branch_manager_live.rs (339 lines)`

**What it is.** A full Neon database-branching integration: branch manager with checkpoint create/rollback/TTL logic in moa-session, an orchestrator maintenance service, admin wiring, DatabaseNeonConfig with 9 env knobs (API key, project ID, parent branch, max checkpoints, TTL, pooled, suspend timeout, enabled), and a live test.

**Why it may be over-engineered.** MOA_DATABASE_NEON_ENABLED (and all 8 sibling knobs) is set nowhere — not in .env.example, compose, k8s, fly, Makefile, scripts, docs, or any test env. `neon_enabled` is referenced only inside the config module. docs/10 lists Neon branching as 'Optional'. This is a vendor-specific checkpoint/rollback capability built ahead of any consumer, carried as always-compiled runtime code plus a maintenance service registered in the orchestrator.

**Simpler alternative.** Delete neon.rs, neon_maint.rs, the admin/jobs wiring, DatabaseNeonConfig, the 9 overlay fields, and the live test. Reintroduce from git history if/when a Neon deployment with checkpointing is actually provisioned.

**Side effects / what to watch.** Loses the (currently unused) DB checkpoint/rollback capability and its live test; docs/10 optional-services table needs a row removed. No environment changes behavior since the feature is disabled everywhere today.

**Value of simplifying.** Deletes ~1,300 lines including a background maintenance service (one fewer registered Restate service and moving part in the orchestrator) and 9 dead config knobs.

**Adversarial verifier: ❌ REFUTED.** The deletion premise is wrong: current `.env` contains Neon settings, edge exposes admin checkpoint routes, and cron registers `neon_prune_branches`. The real issue is inconsistent config naming between `MOA_NEON_API_KEY` and `database.neon.enabled` / `MOA_DATABASE_NEON_API_KEY`; fix naming/config, do not delete the Neon branching stack.

---

### 100. Neon checkpoint/branch subsystem (BranchManager + NeonBranchManager + NeonMaint cron + 4 admin handlers) that is never enabled anywhere

**Area:** session / db / migrations / runtime-store
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~1300**

**Locations**

- `crates/moa-session/src/neon.rs`
- `crates/moa-session/tests/neon_branch_manager_live.rs`
- `crates/moa-orchestrator/src/services/neon_maint.rs`
- `crates/moa-orchestrator/src/services/admin_maintenance.rs:176-290`
- `crates/moa-core/src/traits/mod.rs:555-600`
- `crates/moa-core/src/config/database.rs:52-110`

**What it is.** An 861-line Neon REST-API client that creates/lists/rolls-back/prunes database branches as 'session checkpoints', a BranchManager trait in moa-core, a dedicated NeonMaint Restate cron service registered in the endpoint, four AdminMaintenance checkpoint handlers, a database.neon config section with 8 knobs (project_id, parent_branch_id, max_checkpoints, checkpoint_ttl_hours, pooled, suspend_timeout_seconds, api_key, enabled), and ignored live tests.

**Why it may be over-engineered.** database.neon.enabled defaults to false and nothing in the repo (no config file, compose file, or deploy manifest) sets it true; NeonMaint no-ops without MOA_NEON_API_KEY. No production session flow ever creates a checkpoint — the only create_checkpoint/rollback_to callers are the admin handlers themselves and #[ignore] live tests. For a pre-production product with no external users, a full database-branch rollback console is speculative operational tooling; the BranchManager trait has one impl and is even annotated 'Deliberately not dyn-compatible: no dyn BranchManager usage in the workspace'.

**Simpler alternative.** Delete neon.rs, the live test, NeonMaint (and its endpoint binding), the four checkpoint_* AdminMaintenance handlers, the BranchManager trait plus CheckpointHandle/CheckpointInfo types, and the database.neon config block (including its env overlays). If Neon-branch checkpointing is genuinely wanted later, reintroduce it as one thin admin service when a cloud Neon deployment actually exists.

**Side effects / what to watch.** Loses ready-made admin DB-checkpoint/rollback tooling for a future Neon deployment; docs/01 trait map row for BranchManager and docs/05 mention need updating. If the team runs a Neon environment outside this repo, keep it — confidence is medium for that reason.

**Value of simplifying.** Removes ~1300 lines, a registered Restate service, a cron surface, a moa-core trait, and 8 config knobs that have never held a non-default value.

**Adversarial verifier: ❌ REFUTED.** The claim's code reading is mostly accurate, but its decisive premise — that no cloud Neon deployment exists and nothing enables the feature — is wrong, and the claimant explicitly conditioned the deletion on that premise ("If the team runs a Neon environment outside this repo, keep it"). Evidence: (1) The developer's git-ignored /Users/hwuiwon/Github/moa/.env lines 12-14 contain live Neon credentials: MOA_NEON_API_KEY (real napi_ key), MOA_NEON_PROJECT_ID='muddy-recipe-31989384', and MOA_NEON_DB_URL pointing to an actual ep-*.aws.neon.tech endpoint — a real Neon project exists and is used. (2) Neon is the declared production data plane, not speculation: docker-compose.yml:2 says "Production uses managed Postgres / Neon, not this compose stack"; README.md:7,17,140,186 and architecture.md:24,122,210 describe Postgres/Neon as the cloud storage layer, and README.md:153 lists NeonMaint among the deployed Restate services. (3) The claim understates the wiring it would have to rip out — it missed hidden consumers: moa-edge exposes public admin HTTP routes /v1/admin-maintenance/checkpoints/{create,list,rollback,cleanup} forwarding to the four handlers (crates/moa-edge/src/routes/analytics.rs:475-492, with route-translation tests at 948-964); wire DTOs CheckpointCreateRequest/CheckpointRollbackRequest/etc. live in crates/moa-core/src/wire/admin.rs; a default cron job 'neon_prune_branches' (0,6,12,18 UTC) is registered in crates/moa-orchestrator/src/runtime/jobs.rs:174-184 and documented in docs/02-brain-orchestration.md:350; xtask check_architecture_boundaries.rs:469 allowlists neon_maint.rs; env-overlay coverage exists in crates/moa-core/src/config/mod.rs:259,365; and docs/12, docs/15, README, architecture.md all reference the subsystem — far beyond the docs/01 and docs/05 rows the claim cited. So this is a complete, end-to-end wired, authz-gated (authorize_tenant_admin) operational surface for the actual production database platform, which cleanly no-ops locally (NeonMaint skips when MOA_NEON_API_KEY is unset, crates/moa-orchestrator/src/services/neon_maint.rs:49-59). Deleting it would remove a working admin console for a Neon project the team demonstrably operates. Factual corrections to the claim: neon.rs is 861 lines but roughly a third is #[cfg(test)] mock-server unit tests (TestState at line 558 onward), so the production client is smaller than claimed; and 'the only callers are admin handlers' overlooks that those handlers are reachable via the public edge API. Legitimate smaller cleanups do exist (not grounds for the proposed deletion): NeonBranchManager::maybe_from_config (neon.rs:90) has zero callers, and there are three inconsistent env naming schemes (MOA_NEON_API_KEY gate in neon_maint.rs vs MOA_DATABASE_NEON_API_KEY/MOA_DATABASE_NEON_ENABLED in config, vs NEON_API_KEY/NEON_PROJECT_ID in the live test) — with the current .env the cron gate passes but from_config would terminal-error because MOA_DATABASE_NEON_ENABLED is unset; that is a misconfiguration bug worth fixing, not evidence the subsystem is dead.

---

### 101. Fjall fsync journal wraps the lossy fire-and-forget lineage path that is allowed to drop events anyway

**Area:** lineage / ocsf / observability
effort: **medium** · finder confidence: **medium** · ~LOC removable: **~300**

**Locations**

- `crates/moa-lineage/sink/src/fjall_journal.rs (whole file, 119 lines)`
- `crates/moa-lineage/sink/src/writer.rs:101-233 (DurableJournal: replay cursor, ack_sequences range-merging, spawn_blocking wrappers)`
- `crates/moa-lineage/sink/src/writer.rs:311-460 (run_writer/flush_events/replay_pending journal dance)`
- `crates/moa-lineage/sink/src/mpsc_sink.rs:132-181`

**What it is.** In `MOA_LINEAGE_SINK=postgres` mode, every fire-and-forget telemetry event (retrieval/context/generation lineage) is batched, group-committed to an fsync'd fjall journal, immediately written to Postgres, then deleted from the journal via a second fjall write-tx + persist. Supporting machinery includes a shared DurableJournal with an atomic sequence counter, a low-water-mark replay cursor, ack-range merging over sorted sequence lists, journal-depth gauges recomputed via spawn_blocking after every flush, and `WriterCommand::Journaled` notifications that trigger a full flush+replay per durable event.

**Why it may be over-engineered.** The hot path's own contract (sink.rs doc, mpsc_sink.rs:132-137) is best-effort: events are dropped with a counter when the 8192-deep channel saturates, and events buffered in memory (up to 512 rows / 2s) are lost on process crash before the flush journals them. So the journal only protects the narrow window between dequeue and Postgres-commit — a window already covered by the existing 5-attempt sqlstate-aware retry and the dead-letter table. Two fsyncs plus a delete per batch, plus cursor/ack bookkeeping, buy durability for a stream that is explicitly droppable at admission. Only `record_durable_event` (Decision/Score records awaited by callers) genuinely needs journal-backed acceptance.

**Simpler alternative.** Keep the fjall journal only for the awaited `record_durable_event` path (append -> notify -> replay/ack, which already exists). For `WriterCommand::Event` batches, write straight to Postgres with the existing retry + dead-letter logic and drop the journal append/ack/depth accounting from flush_events. This removes the group-commit append_event_rows, ack_sequences range merging, and per-flush approximate_len_async round-trips; replay_pending shrinks to durable-event recovery on startup and Journaled notifications.

**Side effects / what to watch.** Narrows the documented invariant in docs/02-brain-orchestration.md:118-128 ('queue pressure can drop only before the journal append succeeds') to durable events only — that doc section must be rewritten. writer_db.rs tests (flush-on-shutdown, poison-batch retention, compliance chain) need reworking; if the new chaos durability invariant checker asserts lossy-lineage survival across Postgres outages, that assertion has to move to the durable path.

**Value of simplifying.** Removes two fsyncs and one delete transaction per batch from the steady-state write loop, deletes roughly half the writer's moving parts (cursor, ack merging, depth plumbing), and makes the durability story honest: exactly one path guarantees acceptance.

**Adversarial verifier: ❌ REFUTED.** The claim's load-bearing premise — that the dequeue-to-Postgres-commit window is "already covered by the 5-attempt retry and the dead-letter table" — is factually wrong: the dead-letter table (analytics.lineage_dead_letters, crates/moa-lineage/sink/src/writer.rs:520-569) lives in the same Postgres the write failed against. During a Postgres outage/restart (chaos scenarios exist: crates/moa-loadtest/tests/chaos_docker/combo_storm_pg_restart_docker.rs, postgres_latency_docker.rs), retry exhausts, the dead-letter insert also fails, flush_events errors and the writer dies; only the fsync'd journal preserves the batch, and replay_pending at next writer start delivers it into analytics.turn_lineage. This exact Event-path behavior is pinned by a db test the claimant hand-waved as 'needs reworking': crates/moa-lineage/sink/tests/writer_db.rs:78 lineage_writer_poison_batch_dead_letters_without_acking_journal_db drives a fire-and-forget lossy event, asserts the journal row is retained un-acked after dead-lettering, and asserts a fresh writer replays it exactly once into turn_lineage — dead-letter is a manual-triage JSONB artifact, the journal is the automatic re-delivery path; they are not redundant. The claim also misquotes docs/02-brain-orchestration.md:124-128 (actual text: "Queue pressure can drop only explicitly configured lossy telemetry; audit-class events are not accepted before the journal append succeeds"), and misses a hidden consumer: docs/operations/kubernetes-env.md:53-66 defines the ops metric taxonomy where moa_lineage_accepted_total{durability="journal"} counts ALL events after journal append (record_journal_acceptance fires on both paths, writer.rs:143/173) and drop counters are admission-only; removing Event-path journaling makes post-dequeue crash loss silent and uncounted, breaking the documented "loss only at admission, always counted" accounting. The cost framing is also misleading: group commit (Journal::append_batch, one SyncData persist per up-to-512-row/2s batch, pinned by append_batch_groups_appends_into_one_persist) plus one ack persist runs on a background spawn_blocking thread — not a hot-path cost. Finally the simplification yield is small: the durable record_durable_event path the claimant keeps already requires Journal, DurableJournal, the sequence counter, replay cursor, replay_pending, ack_range, spawn_blocking wrappers, and Journaled notifications; deleting Event-path journaling removes only ~60-70 lines (append_event_rows, ack_sequences, append_batch, a few flush_events lines) while destroying tested outage recovery. Two minor kernels of truth that do not rescue the claim: (a) ack_sequences range-merging (writer.rs:197-216) is over-general since flush-batch sequences are assigned contiguously under one lock guard and always collapse to a single ack_range — a ~15-line micro-cleanup; (b) no chaos invariant checker currently asserts lineage-journal survival (no lineage/journal references in crates/moa-loadtest/src/scenarios/chaos/mod.rs or docs/22-load-and-chaos-testing.md), so that claimed side effect is moot.

---
