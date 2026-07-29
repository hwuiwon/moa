//! Durable Restate façade over the PostgreSQL-backed MOA session store.
//!
//! This module intentionally exposes the Restate service as `SessionStore` while
//! keeping the implementation separate from `moa_core::traits::SessionStore`. The S05
//! audit classified this as a workflow RPC facade, not a duplicate core trait.

use std::sync::Arc;

use moa_core::traits::{EmbeddingProvider, RuntimeCacheStore};
use moa_core::{
    events::Event, traits::SessionStore as CoreSessionStore, types::events_stream::EventRecord,
    types::experience::ExperienceAttribution, types::experience::ExperienceRecord,
    types::experience::LearningCandidate, types::experience::TaskStrategySuccessRate,
    types::identifiers::SessionId, types::segment_assessment::SegmentBaseline,
    types::segment_assessment::SkillResolutionRate, types::segments::TaskSegment,
    types::session::SessionMeta, types::session::SessionSummary,
};
use moa_observability::record_session_error;
use moa_session::PostgresSessionStore;
use moa_wire::session_store::{
    AppendEventRequest, AppendExperienceAttributionsRequest, AppendExperienceRecordRequest,
    AppendLearningCandidateRequest, CompleteSegmentRequest, CreateAgentSessionRequest,
    CreateAgentSessionResponse, CreateSegmentRequest, GetEventsRequest,
    GetLearningCandidateRequest, GetSegmentBaselineRequest, InitSessionVoRequest,
    ListExperienceAttributionsRequest, ListExperienceRecordsRequest, ListLearningCandidatesRequest,
    ListSessionsRequest, ListSkillResolutionRatesRequest, ListTaskStrategySuccessRatesRequest,
    RecordSegmentSkillActivationRequest, RecordSegmentSkillUseRequest, RecordSegmentToolUseRequest,
    RecordSegmentTurnUsageRequest, SearchEventsRequest, TenantCostSinceRequest,
    UpdateSegmentAssessmentRequest, UpdateStatusRequest,
};
use restate_sdk::prelude::*;
use sqlx::PgPool;

use crate::objects::session::SessionClient;
use crate::workflows::session_retention::{SessionRetentionDispatch, SessionRetentionRequest};
use moa_observability::restate_observability::annotate_restate_handler_span;

mod handlers;
pub(crate) mod inner;
#[cfg(test)]
mod tests;

/// Restate service surface for durable session/event storage.
#[restate_sdk::service]
#[name = "SessionStore"]
pub trait RestateSessionStore {
    /// Persists a session metadata row.
    async fn create_session(meta: Json<SessionMeta>) -> Result<Json<SessionId>, HandlerError>;

    /// Resolves an installed or exact agent revision and persists a pinned session row.
    async fn create_agent_session(
        request: Json<CreateAgentSessionRequest>,
    ) -> Result<Json<CreateAgentSessionResponse>, HandlerError>;

    /// Appends one event to the durable session log and returns its stored record.
    async fn append_event(
        request: Json<AppendEventRequest>,
    ) -> Result<Json<EventRecord>, HandlerError>;

    /// Loads events from one session within a requested range.
    async fn get_events(
        request: Json<GetEventsRequest>,
    ) -> Result<Json<Vec<EventRecord>>, HandlerError>;

    /// Loads one persisted session metadata row.
    async fn get_session(session_id: Json<SessionId>) -> Result<Json<SessionMeta>, HandlerError>;

    /// Updates the persisted lifecycle status for one session.
    async fn update_status(request: Json<UpdateStatusRequest>) -> Result<(), HandlerError>;

    /// Searches persisted events using the backend full-text index.
    async fn search_events(
        request: Json<SearchEventsRequest>,
    ) -> Result<Json<Vec<EventRecord>>, HandlerError>;

    /// Lists persisted session summaries matching the provided filter.
    async fn list_sessions(
        request: Json<ListSessionsRequest>,
    ) -> Result<Json<Vec<SessionSummary>>, HandlerError>;

    /// Aggregates tenant spend since the requested timestamp.
    async fn tenant_cost_since(request: Json<TenantCostSinceRequest>) -> Result<u32, HandlerError>;

    /// Bootstraps VO state after the session row exists in Postgres.
    async fn init_session_vo(request: Json<InitSessionVoRequest>) -> Result<(), HandlerError>;

    /// Persists a task segment row.
    async fn create_segment(request: Json<CreateSegmentRequest>) -> Result<(), HandlerError>;

    /// Completes a task segment row.
    async fn complete_segment(request: Json<CompleteSegmentRequest>) -> Result<(), HandlerError>;

    /// Loads the active task segment for a session.
    async fn get_active_segment(
        session_id: Json<SessionId>,
    ) -> Result<Json<Option<TaskSegment>>, HandlerError>;

    /// Lists task segments for a session.
    async fn list_segments(
        session_id: Json<SessionId>,
    ) -> Result<Json<Vec<TaskSegment>>, HandlerError>;

    /// Updates a task segment assessment artifact.
    async fn update_segment_assessment(
        request: Json<UpdateSegmentAssessmentRequest>,
    ) -> Result<(), HandlerError>;

    /// Loads a task-segment structural baseline.
    async fn get_segment_baseline(
        request: Json<GetSegmentBaselineRequest>,
    ) -> Result<Json<Option<SegmentBaseline>>, HandlerError>;

    /// Lists skill resolution-rate aggregates.
    async fn list_skill_resolution_rates(
        request: Json<ListSkillResolutionRatesRequest>,
    ) -> Result<Json<Vec<SkillResolutionRate>>, HandlerError>;

    /// Lists task-conditioned strategy success aggregates.
    async fn list_task_strategy_success_rates(
        request: Json<ListTaskStrategySuccessRatesRequest>,
    ) -> Result<Json<Vec<TaskStrategySuccessRate>>, HandlerError>;

    /// Appends or refreshes one experience record.
    async fn append_experience_record(
        request: Json<AppendExperienceRecordRequest>,
    ) -> Result<(), HandlerError>;

    /// Lists experience records for one session.
    async fn list_experience_records(
        request: Json<ListExperienceRecordsRequest>,
    ) -> Result<Json<Vec<ExperienceRecord>>, HandlerError>;

    /// Appends or refreshes experience attributions.
    async fn append_experience_attributions(
        request: Json<AppendExperienceAttributionsRequest>,
    ) -> Result<(), HandlerError>;

    /// Lists attributions for one experience.
    async fn list_experience_attributions(
        request: Json<ListExperienceAttributionsRequest>,
    ) -> Result<Json<Vec<ExperienceAttribution>>, HandlerError>;

    /// Appends or refreshes one learning candidate.
    async fn append_learning_candidate(
        request: Json<AppendLearningCandidateRequest>,
    ) -> Result<(), HandlerError>;

    /// Loads one full learning candidate by tenant and candidate ID.
    async fn get_learning_candidate(
        request: Json<GetLearningCandidateRequest>,
    ) -> Result<Json<LearningCandidate>, HandlerError>;

    /// Lists learning candidates for a tenant.
    async fn list_learning_candidates(
        request: Json<ListLearningCandidatesRequest>,
    ) -> Result<Json<Vec<LearningCandidate>>, HandlerError>;

    /// Starts a durable terminal-session retention pass for one tenant.
    async fn start_session_retention(
        request: Json<SessionRetentionRequest>,
    ) -> Result<Json<SessionRetentionDispatch>, HandlerError>;

    /// Refreshes materialized views derived from task segments.
    async fn refresh_segment_materialized_views(
        request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError>;

    /// Refreshes the analytics materialized views under a single-flight lease.
    async fn refresh_analytics_materialized_views(
        request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError>;

    /// Files rollback proposals for skills that regressed after promotion.
    async fn monitor_skill_regressions(
        request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError>;

    /// Backfills learning embeddings (task summaries and skill identities).
    async fn backfill_learning_embeddings(
        request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError>;

    /// Dispatches skill learning for task fingerprints that recur across sessions.
    async fn mine_task_recurrences(request: Json<serde_json::Value>) -> Result<(), HandlerError>;

    /// Records a tool name on a session's active segment.
    async fn record_segment_tool_use(
        request: Json<RecordSegmentToolUseRequest>,
    ) -> Result<(), HandlerError>;

    /// Records a skill activation (injection) on a session's active segment.
    async fn record_segment_skill_activation(
        request: Json<RecordSegmentSkillActivationRequest>,
    ) -> Result<(), HandlerError>;

    /// Records that the model engaged a skill on a session's active segment.
    async fn record_segment_skill_use(
        request: Json<RecordSegmentSkillUseRequest>,
    ) -> Result<(), HandlerError>;

    /// Records one turn and token usage on a session's active segment.
    async fn record_segment_turn_usage(
        request: Json<RecordSegmentTurnUsageRequest>,
    ) -> Result<(), HandlerError>;
}

/// Concrete Restate service implementation backed by `PostgresSessionStore`.
#[derive(Clone)]
pub struct SessionStoreImpl {
    store: Arc<PostgresSessionStore>,
    pool: PgPool,
    config: Arc<moa_config::MoaConfig>,
    /// Tenant embedder reused for the learning-embeddings backfill cron. `None`
    /// when the configured vector embedder is disabled or its credential is
    /// missing; the backfill handler then no-ops so a deployment without
    /// embeddings still runs. Built once so the provider's pacer and concurrency
    /// limiter are shared across ticks.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

impl SessionStoreImpl {
    /// Creates a new Restate service wrapper around the shared session-store backend.
    pub fn new(
        store: Arc<PostgresSessionStore>,
        pool: PgPool,
        config: Arc<moa_config::MoaConfig>,
        runtime_cache: Arc<dyn RuntimeCacheStore>,
    ) -> Self {
        let embedder = build_learning_embedder(&config, runtime_cache);
        Self {
            store,
            pool,
            config,
            embedder,
        }
    }
}

/// Builds the tenant embedder used by the learning-embeddings backfill.
///
/// Reuses the same `memory.vector.embedder` selector and 1024-dim output the
/// graph-memory vector index uses, so learning embeddings share one vector
/// space with memory. A disabled selector or a missing credential is not a
/// startup error here: it disables the backfill and logs a warning, matching how
/// semantic memory search degrades when the embedder is unavailable.
fn build_learning_embedder(
    config: &moa_config::MoaConfig,
    runtime_cache: Arc<dyn RuntimeCacheStore>,
) -> Option<Arc<dyn EmbeddingProvider>> {
    match moa_providers::embedding::build_embedder_from_config(
        config,
        Some(runtime_cache),
        moa_providers::EmbedderConstructionRole::Ingestion,
    ) {
        Ok(embedder) => Some(embedder),
        Err(error) => {
            tracing::warn!(
                %error,
                "learning-embeddings backfill disabled: tenant embedder unavailable"
            );
            None
        }
    }
}
