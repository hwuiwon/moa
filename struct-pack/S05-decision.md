# S05 Decision Document

## Verdict

LEAVE.

`crates/moa-orchestrator/src/services/session_store.rs` does not currently define
a Rust trait named `SessionStore`. It defines `RestateSessionStore`, a Restate
RPC service trait whose external Restate service name is intentionally
`SessionStore` via `#[name = "SessionStore"]`. The core storage trait remains
`moa_core::traits::SessionStore`.

This is an intentional Restate facade over the Postgres-backed core session
store, not a duplicate trait that should be deleted or merged.

## Evidence

### Trait definition comparison

| Method | `moa_core::traits::SessionStore` | `moa_orchestrator::services::session_store::RestateSessionStore` | Notes |
|---|---|---|---|
| `create_session` | `async fn create_session(&self, meta: SessionMeta) -> Result<SessionId>` | `async fn create_session(meta: Json<SessionMeta>) -> Result<Json<SessionId>, HandlerError>` | WIDENED: same storage action, wrapped for Restate JSON and handler errors. |
| `emit_event` / `append_event` | `async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum>` | `async fn append_event(request: Json<AppendEventRequest>) -> Result<u64, HandlerError>` | RENAMED: Restate endpoint packages `session_id` and `event`; implementation calls `emit_event`. |
| `store_text_artifact` | default unsupported text artifact storage | Missing | MISSING: not exposed through the Restate service. |
| `load_text_artifact` | default unsupported text artifact load | Missing | MISSING: not exposed through the Restate service. |
| `get_events` | `async fn get_events(&self, session_id: SessionId, range: EventRange) -> Result<Vec<EventRecord>>` | `async fn get_events(request: Json<GetEventsRequest>) -> Result<Json<Vec<EventRecord>>, HandlerError>` | WIDENED: same storage action, request-object and JSON wrapped. |
| `get_session` | `async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta>` | `async fn get_session(session_id: Json<SessionId>) -> Result<Json<SessionMeta>, HandlerError>` | WIDENED: same storage action, JSON wrapped. |
| `update_status` | `async fn update_status(&self, session_id: SessionId, status: SessionStatus) -> Result<()>` | `async fn update_status(request: Json<UpdateStatusRequest>) -> Result<(), HandlerError>` | WIDENED: same storage action, request-object wrapped. |
| `transition_status` | default method that updates status and emits `SessionStatusChanged` | Missing | MISSING: core convenience behavior is not exposed as a Restate endpoint. |
| `put_snapshot` | default no-op compiled-context snapshot write | Missing | MISSING: not exposed through the Restate service. |
| `get_snapshot` | default no-op compiled-context snapshot read | Missing | MISSING: not exposed through the Restate service. |
| `delete_snapshot` | default no-op compiled-context snapshot delete | Missing | MISSING: not exposed through the Restate service. |
| `store_pending_signal` | durable pending signal write | Missing | MISSING: not exposed through the Restate service. |
| `get_pending_signals` | durable pending signal read | Missing | MISSING: not exposed through the Restate service. |
| `resolve_pending_signal` | durable pending signal resolution | Missing | MISSING: not exposed through the Restate service. |
| `search_events` | `async fn search_events(&self, query: &str, filter: EventFilter) -> Result<Vec<EventRecord>>` | `async fn search_events(request: Json<SearchEventsRequest>) -> Result<Json<Vec<EventRecord>>, HandlerError>` | WIDENED: same storage action, request-object and JSON wrapped. |
| `list_sessions` | session summary listing | Missing | MISSING: not exposed through the Restate service. |
| `workspace_cost_since` | workspace spend aggregate | Missing | MISSING: not exposed through the Restate service. |
| `delete_session` | durable session deletion | Missing | MISSING: not exposed through the Restate service. |
| `create_segment` | `async fn create_segment(&self, segment: &TaskSegment) -> Result<()>` | `async fn create_segment(request: Json<CreateSegmentRequest>) -> Result<(), HandlerError>` | WIDENED: same storage action, request-object wrapped. |
| `complete_segment` | `async fn complete_segment(&self, segment_id: SegmentId, update: SegmentCompletion) -> Result<()>` | `async fn complete_segment(request: Json<CompleteSegmentRequest>) -> Result<(), HandlerError>` | WIDENED: same storage action, request-object wrapped. |
| `get_active_segment` | `async fn get_active_segment(&self, session_id: SessionId) -> Result<Option<TaskSegment>>` | `async fn get_active_segment(session_id: Json<SessionId>) -> Result<Json<Option<TaskSegment>>, HandlerError>` | WIDENED: same storage action, JSON wrapped. |
| `list_segments` | `async fn list_segments(&self, session_id: SessionId) -> Result<Vec<TaskSegment>>` | `async fn list_segments(session_id: Json<SessionId>) -> Result<Json<Vec<TaskSegment>>, HandlerError>` | WIDENED: same storage action, JSON wrapped. |
| `update_segment_resolution` | `async fn update_segment_resolution(&self, segment_id: SegmentId, resolution: &str, confidence: f64) -> Result<()>` | `async fn update_segment_resolution(request: Json<UpdateSegmentResolutionRequest>) -> Result<(), HandlerError>` | WIDENED: same storage action, request-object wrapped. |
| `update_segment_resolution_score` | `async fn update_segment_resolution_score(&self, segment_id: SegmentId, score: &ResolutionScore) -> Result<()>` | `async fn update_segment_resolution_score(request: Json<UpdateSegmentResolutionScoreRequest>) -> Result<(), HandlerError>` | WIDENED: same storage action, request-object wrapped. |
| `get_segment_baseline` | `async fn get_segment_baseline(&self, tenant_id: &str, intent_label: Option<&str>) -> Result<Option<SegmentBaseline>>` | `async fn get_segment_baseline(request: Json<GetSegmentBaselineRequest>) -> Result<Json<Option<SegmentBaseline>>, HandlerError>` | WIDENED: same storage action, owned request fields for RPC serialization. |
| `list_skill_resolution_rates` | `async fn list_skill_resolution_rates(&self, tenant_id: &str, intent_label: Option<&str>) -> Result<Vec<SkillResolutionRate>>` | `async fn list_skill_resolution_rates(request: Json<ListSkillResolutionRatesRequest>) -> Result<Json<Vec<SkillResolutionRate>>, HandlerError>` | WIDENED: same storage action, owned request fields for RPC serialization. |
| `refresh_segment_materialized_views` | `async fn refresh_segment_materialized_views(&self) -> Result<()>` | `async fn refresh_segment_materialized_views() -> Result<(), HandlerError>` | WIDENED: same storage action, handler error type. |
| `record_active_segment_tool_use` / `record_segment_tool_use` | `async fn record_active_segment_tool_use(&self, session_id: SessionId, tool_name: &str) -> Result<()>` | `async fn record_segment_tool_use(request: Json<RecordSegmentToolUseRequest>) -> Result<(), HandlerError>` | RENAMED: endpoint name omits `active`; implementation calls the core method. |
| `record_active_segment_skill_activation` / `record_segment_skill_activation` | `async fn record_active_segment_skill_activation(&self, session_id: SessionId, skill_name: &str) -> Result<()>` | `async fn record_segment_skill_activation(request: Json<RecordSegmentSkillActivationRequest>) -> Result<(), HandlerError>` | RENAMED: endpoint name omits `active`; implementation calls the core method. |
| `record_active_segment_turn_usage` / `record_segment_turn_usage` | `async fn record_active_segment_turn_usage(&self, session_id: SessionId, token_cost: u64) -> Result<()>` | `async fn record_segment_turn_usage(request: Json<RecordSegmentTurnUsageRequest>) -> Result<(), HandlerError>` | RENAMED: endpoint name omits `active`; implementation calls the core method. |
| Missing | No equivalent core method | `async fn init_session_vo(request: Json<InitSessionVoRequest>) -> Result<(), HandlerError>` | DIVERGED: Restate-specific VO bootstrap after Postgres metadata is created. |

### Caller list

Files using the core trait or core trait objects:

- `crates/moa-brain/examples/chat_harness.rs`
- `crates/moa-brain/src/compaction.rs`
- `crates/moa-brain/src/harness/approval_flow.rs`
- `crates/moa-brain/src/harness/budget.rs`
- `crates/moa-brain/src/harness/context_build.rs`
- `crates/moa-brain/src/harness/mod.rs`
- `crates/moa-brain/src/harness/streaming.rs`
- `crates/moa-brain/src/harness/tool_dispatch.rs`
- `crates/moa-brain/src/intents/classifier.rs`
- `crates/moa-brain/src/pipeline/compactor.rs`
- `crates/moa-brain/src/pipeline/history.rs`
- `crates/moa-brain/src/pipeline/mod.rs`
- `crates/moa-brain/src/pipeline/query_rewrite.rs`
- `crates/moa-brain/src/pipeline/segments.rs`
- `crates/moa-brain/src/pipeline/skills.rs`
- `crates/moa-brain/tests/brain_turn.rs`
- `crates/moa-brain/tests/integration_steps_72_77.rs`
- `crates/moa-brain/tests/live_cache_audit.rs`
- `crates/moa-brain/tests/stable_prefix.rs`
- `crates/moa-cli/src/daemon.rs`
- `crates/moa-cli/src/main.rs`
- `crates/moa-core/src/session_replay.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-eval/src/setup.rs`
- `crates/moa-hands/src/router/construction.rs`
- `crates/moa-hands/src/router/mod.rs`
- `crates/moa-hands/src/tools/tool_result.rs`
- `crates/moa-hands/tests/local_tools.rs`
- `crates/moa-orchestrator-local/src/lib.rs`
- `crates/moa-orchestrator-local/tests/live_observability.rs`
- `crates/moa-orchestrator-local/tests/live_provider_roundtrip.rs`
- `crates/moa-orchestrator-local/tests/local_orchestrator.rs`
- `crates/moa-orchestrator-local/tests/prometheus_metrics.rs`
- `crates/moa-orchestrator/src/brain_bridge.rs`
- `crates/moa-orchestrator/src/ctx.rs`
- `crates/moa-orchestrator/src/services/intent_manager.rs`
- `crates/moa-orchestrator/src/services/session_store.rs`
- `crates/moa-orchestrator/tests/support/session_store_service.rs`
- `crates/moa-session/src/lib.rs`
- `crates/moa-session/src/listener.rs`
- `crates/moa-session/src/store.rs`
- `crates/moa-skills/src/distiller.rs`
- `crates/moa-skills/src/improver.rs`

Files using the Restate service trait, generated client, implementation, or
`SessionStore/...` HTTP endpoint names:

- `crates/moa-orchestrator/src/main.rs`
- `crates/moa-orchestrator/src/objects/session.rs`
- `crates/moa-orchestrator/src/objects/sub_agent.rs`
- `crates/moa-orchestrator/src/services/llm_gateway.rs`
- `crates/moa-orchestrator/src/services/session_store.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- `crates/moa-orchestrator/src/turn/approval.rs`
- `crates/moa-orchestrator/src/turn/runner.rs`
- `crates/moa-orchestrator/tests/integration/approval_flow_e2e.rs`
- `crates/moa-orchestrator/tests/integration/session_brain_e2e.rs`
- `crates/moa-orchestrator/tests/integration/session_store_e2e.rs`
- `crates/moa-orchestrator/tests/integration/session_vo_e2e.rs`
- `crates/moa-orchestrator/tests/integration/tool_executor_e2e.rs`
- `crates/moa-orchestrator/tests/llm_gateway_e2e.rs`
- `crates/moa-orchestrator/tests/support/session_store_service.rs`

No file uses `moa_orchestrator::services::SessionStore`, and a search for public
session-store traits found only:

- `crates/moa-core/src/traits/mod.rs`: `pub trait SessionStore`
- `crates/moa-orchestrator/src/services/session_store.rs`: `pub trait RestateSessionStore`

### Impl pattern observed

- `crates/moa-session/src/store.rs` implements `moa_core::traits::SessionStore`
  for `PostgresSessionStore`.
- `crates/moa-orchestrator/src/services/session_store.rs` implements
  `RestateSessionStore` for `SessionStoreImpl`.
- `SessionStoreImpl` holds `Arc<PostgresSessionStore>`, imports the core trait
  as `SessionStore as CoreSessionStore`, and delegates almost every endpoint to
  the corresponding core trait method on the Postgres store.
- No struct implements both `moa_core::traits::SessionStore` and
  `RestateSessionStore`.
- The one non-core endpoint is `init_session_vo`, which calls the Restate
  `Session` virtual object client and has no storage-trait equivalent.

This is the decorator/facade pattern, not an independent storage abstraction.

### Doc comments excerpted

- `moa_core::traits::SessionStore`: "Durable append-only session store."
- `moa-orchestrator/src/services/session_store.rs` module: "Durable Restate
  facade over the PostgreSQL-backed MOA session store."
- `RestateSessionStore`: "Restate service surface for durable session/event
  storage."
- `SessionStoreImpl`: "Concrete Restate service implementation backed by
  `PostgresSessionStore`."

The file-level and trait-level comments explicitly identify the orchestrator
surface as a Restate facade.

## Recommendation for S11

Do not delete or merge `RestateSessionStore`. It is not a duplicate of
`moa_core::traits::SessionStore`; it is the RPC boundary for Restate handlers.

Do not rename the external Restate service name `#[name = "SessionStore"]`.
That name is part of the live Restate HTTP/API surface and is referenced by E2E
tests and service registration checks.

Do not promote `init_session_vo` into the core trait. It is Restate-specific VO
state initialization, not durable session storage.

If S11 still wants a clarity-only cleanup, the safe action is limited to comments
or documentation explaining that `RestateSessionStore` is the Restate service
facade for the core `SessionStore`. The Rust symbol name is already
disambiguated, so no code rename is required for this suspicion.

## Risk assessment

Deleting the Restate trait would remove the generated
`RestateSessionStoreClient` and `.serve()` implementation required by the
orchestrator service graph.

Renaming the external Restate service from `SessionStore` would break direct
HTTP E2E calls such as `SessionStore/create_session`, service-discovery
expectations, and any deployed Restate registration that expects that service
name.

Replacing Restate calls with direct `moa_core::traits::SessionStore` calls would
change durability boundaries. Current orchestrator objects call the Restate
service through `ctx.service_client::<RestateSessionStoreClient>()`; bypassing
that would skip the intended service boundary and could alter replay behavior.

Merging `init_session_vo` into the core trait would leak Restate virtual-object
state concerns into the storage abstraction and make the core trait less
portable for local/runtime implementations.
