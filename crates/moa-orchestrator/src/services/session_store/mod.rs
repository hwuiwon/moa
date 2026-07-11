//! Durable Restate façade over the PostgreSQL-backed MOA session store.
//!
//! This module intentionally exposes the Restate service as `SessionStore` while
//! keeping the implementation separate from `moa_core::traits::SessionStore`. The S05
//! audit classified this as a workflow RPC facade, not a duplicate core trait.

use std::sync::Arc;

use moa_core::wire::session_store::{
    AppendEventRequest, AppendExperienceAttributionsRequest, AppendExperienceRecordRequest,
    AppendLearningCandidateRequest, CompleteSegmentRequest, CreateAgentSessionRequest,
    CreateAgentSessionResponse, CreateSegmentRequest, GetEventsRequest,
    GetLearningCandidateRequest, GetSegmentBaselineRequest, InitSessionVoRequest,
    ListExperienceAttributionsRequest, ListExperienceRecordsRequest, ListLearningCandidatesRequest,
    ListSessionsRequest, ListSkillResolutionRatesRequest, ListTaskStrategySuccessRatesRequest,
    RecordSegmentSkillActivationRequest, RecordSegmentToolUseRequest,
    RecordSegmentTurnUsageRequest, SearchEventsRequest, TenantCostSinceRequest,
    UpdateLearningCandidateStatusRequest, UpdateSegmentAssessmentRequest, UpdateStatusRequest,
};
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
use restate_sdk::prelude::*;
use sqlx::PgPool;

use crate::objects::session::SessionClient;
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

    /// Applies a learning-candidate status transition.
    async fn update_learning_candidate_status(
        request: Json<UpdateLearningCandidateStatusRequest>,
    ) -> Result<(), HandlerError>;

    /// Refreshes materialized views derived from task segments.
    async fn refresh_segment_materialized_views(
        request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError>;

    /// Refreshes the analytics materialized views under a single-flight lease.
    async fn refresh_analytics_materialized_views(
        request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError>;

    /// Records a tool name on a session's active segment.
    async fn record_segment_tool_use(
        request: Json<RecordSegmentToolUseRequest>,
    ) -> Result<(), HandlerError>;

    /// Records a skill activation on a session's active segment.
    async fn record_segment_skill_activation(
        request: Json<RecordSegmentSkillActivationRequest>,
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
}

impl SessionStoreImpl {
    /// Creates a new Restate service wrapper around the shared session-store backend.
    pub fn new(store: Arc<PostgresSessionStore>, pool: PgPool) -> Self {
        Self { store, pool }
    }
}
