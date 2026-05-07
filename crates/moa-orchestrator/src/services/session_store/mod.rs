//! Durable Restate façade over the PostgreSQL-backed MOA session store.
//!
//! This module intentionally exposes the Restate service as `SessionStore` while
//! keeping the implementation separate from `moa_core::SessionStore`. The S05
//! audit classified this as a workflow RPC facade, not a duplicate core trait.

use std::sync::Arc;

use moa_core::{
    Event, EventFilter, EventRange, EventRecord, ResolutionScore, SegmentBaseline,
    SegmentCompletion, SegmentId, SessionId, SessionMeta, SessionStatus,
    SessionStore as CoreSessionStore, SkillResolutionRate, TaskSegment, record_session_error,
};
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;

use crate::objects::session::SessionClient;
use crate::observability::annotate_restate_handler_span;

mod handlers;
mod inner;
mod requests;
#[cfg(test)]
mod tests;

pub use requests::{
    AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest, GetEventsRequest,
    GetSegmentBaselineRequest, InitSessionVoRequest, ListSkillResolutionRatesRequest,
    RecordSegmentSkillActivationRequest, RecordSegmentToolUseRequest,
    RecordSegmentTurnUsageRequest, SearchEventsRequest, UpdateSegmentResolutionRequest,
    UpdateSegmentResolutionScoreRequest, UpdateStatusRequest,
};

/// Restate service surface for durable session/event storage.
#[restate_sdk::service]
#[name = "SessionStore"]
pub trait RestateSessionStore {
    /// Persists a session metadata row.
    async fn create_session(meta: Json<SessionMeta>) -> Result<Json<SessionId>, HandlerError>;

    /// Appends one event to the durable session log.
    async fn append_event(request: Json<AppendEventRequest>) -> Result<u64, HandlerError>;

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

    /// Updates a task segment resolution.
    async fn update_segment_resolution(
        request: Json<UpdateSegmentResolutionRequest>,
    ) -> Result<(), HandlerError>;

    /// Updates a task segment resolution and signal breakdown.
    async fn update_segment_resolution_score(
        request: Json<UpdateSegmentResolutionScoreRequest>,
    ) -> Result<(), HandlerError>;

    /// Loads a task-segment structural baseline.
    async fn get_segment_baseline(
        request: Json<GetSegmentBaselineRequest>,
    ) -> Result<Json<Option<SegmentBaseline>>, HandlerError>;

    /// Lists skill resolution-rate aggregates.
    async fn list_skill_resolution_rates(
        request: Json<ListSkillResolutionRatesRequest>,
    ) -> Result<Json<Vec<SkillResolutionRate>>, HandlerError>;

    /// Refreshes materialized views derived from task segments.
    async fn refresh_segment_materialized_views() -> Result<(), HandlerError>;

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
}

impl SessionStoreImpl {
    /// Creates a new Restate service wrapper around the shared session-store backend.
    pub fn new(store: Arc<PostgresSessionStore>) -> Self {
        Self { store }
    }
}
