//! Durable Restate façade over the PostgreSQL-backed MOA session store.

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

/// Request payload for `SessionStore/append_event`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppendEventRequest {
    /// Session receiving the event.
    pub session_id: SessionId,
    /// Event payload to append to the durable log.
    pub event: Event,
}

/// Request payload for `SessionStore/get_events`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GetEventsRequest {
    /// Session whose event log should be read.
    pub session_id: SessionId,
    /// Range and filter options for the event query.
    pub range: EventRange,
}

/// Request payload for `SessionStore/update_status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateStatusRequest {
    /// Session whose lifecycle state should be updated.
    pub session_id: SessionId,
    /// New session lifecycle state.
    pub status: SessionStatus,
}

/// Request payload for `SessionStore/search_events`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchEventsRequest {
    /// Full-text search query.
    pub query: String,
    /// Additional event-search scoping and limits.
    pub filter: EventFilter,
}

/// Request payload for `SessionStore/init_session_vo`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InitSessionVoRequest {
    /// Session object key that should be initialized.
    pub session_id: SessionId,
    /// Session metadata mirrored into Restate object state.
    pub meta: SessionMeta,
}

/// Request payload for `SessionStore/create_segment`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CreateSegmentRequest {
    /// Segment metadata to persist.
    pub segment: TaskSegment,
}

/// Request payload for `SessionStore/complete_segment`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompleteSegmentRequest {
    /// Segment identifier to complete.
    pub segment_id: SegmentId,
    /// Completion counters and end timestamp.
    pub update: SegmentCompletion,
}

/// Request payload for `SessionStore/update_segment_resolution`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UpdateSegmentResolutionRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Resolution label.
    pub resolution: String,
    /// Resolution confidence.
    pub confidence: f64,
}

/// Request payload for `SessionStore/update_segment_resolution_score`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UpdateSegmentResolutionScoreRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Full resolution score and signal breakdown.
    pub score: ResolutionScore,
}

/// Request payload for `SessionStore/get_segment_baseline`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GetSegmentBaselineRequest {
    /// Tenant/workspace identifier.
    pub tenant_id: String,
    /// Optional intent label.
    pub intent_label: Option<String>,
}

/// Request payload for `SessionStore/list_skill_resolution_rates`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListSkillResolutionRatesRequest {
    /// Tenant/workspace identifier.
    pub tenant_id: String,
    /// Optional intent label.
    pub intent_label: Option<String>,
}

/// Request payload for recording active-segment tool usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordSegmentToolUseRequest {
    /// Session whose active segment receives the tool usage.
    pub session_id: SessionId,
    /// Tool name to record.
    pub tool_name: String,
}

/// Request payload for recording active-segment skill usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordSegmentSkillActivationRequest {
    /// Session whose active segment receives the skill activation.
    pub session_id: SessionId,
    /// Skill name to record.
    pub skill_name: String,
}

/// Request payload for recording active-segment turn usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordSegmentTurnUsageRequest {
    /// Session whose active segment receives the turn usage.
    pub session_id: SessionId,
    /// Token cost to add for the turn.
    pub token_cost: u64,
}

/// Restate service surface for durable session/event storage.
#[restate_sdk::service]
pub trait SessionStore {
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

    async fn create_session_inner(&self, meta: SessionMeta) -> Result<SessionId, HandlerError> {
        self.store
            .create_session(meta)
            .await
            .map_err(HandlerError::from)
    }

    async fn append_event_inner(&self, request: AppendEventRequest) -> Result<u64, HandlerError> {
        if matches!(&request.event, Event::Error { .. }) {
            record_session_error("event_log");
        }
        self.store
            .emit_event(request.session_id, request.event)
            .await
            .map_err(HandlerError::from)
    }

    async fn get_events_inner(
        &self,
        request: GetEventsRequest,
    ) -> Result<Vec<EventRecord>, HandlerError> {
        self.store
            .get_events(request.session_id, request.range)
            .await
            .map_err(HandlerError::from)
    }

    async fn get_session_inner(&self, session_id: SessionId) -> Result<SessionMeta, HandlerError> {
        self.store
            .get_session(session_id)
            .await
            .map_err(HandlerError::from)
    }

    async fn update_status_inner(&self, request: UpdateStatusRequest) -> Result<(), HandlerError> {
        self.store
            .update_status(request.session_id, request.status)
            .await
            .map_err(HandlerError::from)
    }

    async fn search_events_inner(
        &self,
        request: SearchEventsRequest,
    ) -> Result<Vec<EventRecord>, HandlerError> {
        self.store
            .search_events(&request.query, request.filter)
            .await
            .map_err(HandlerError::from)
    }

    async fn create_segment_inner(
        &self,
        request: CreateSegmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .create_segment(&request.segment)
            .await
            .map_err(HandlerError::from)
    }

    async fn complete_segment_inner(
        &self,
        request: CompleteSegmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .complete_segment(request.segment_id, request.update)
            .await
            .map_err(HandlerError::from)
    }

    async fn get_active_segment_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Option<TaskSegment>, HandlerError> {
        self.store
            .get_active_segment(session_id)
            .await
            .map_err(HandlerError::from)
    }

    async fn list_segments_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<TaskSegment>, HandlerError> {
        self.store
            .list_segments(session_id)
            .await
            .map_err(HandlerError::from)
    }

    async fn update_segment_resolution_inner(
        &self,
        request: UpdateSegmentResolutionRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_segment_resolution(request.segment_id, &request.resolution, request.confidence)
            .await
            .map_err(HandlerError::from)
    }

    async fn update_segment_resolution_score_inner(
        &self,
        request: UpdateSegmentResolutionScoreRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_segment_resolution_score(request.segment_id, &request.score)
            .await
            .map_err(HandlerError::from)
    }

    async fn get_segment_baseline_inner(
        &self,
        request: GetSegmentBaselineRequest,
    ) -> Result<Option<SegmentBaseline>, HandlerError> {
        self.store
            .get_segment_baseline(&request.tenant_id, request.intent_label.as_deref())
            .await
            .map_err(HandlerError::from)
    }

    async fn list_skill_resolution_rates_inner(
        &self,
        request: ListSkillResolutionRatesRequest,
    ) -> Result<Vec<SkillResolutionRate>, HandlerError> {
        self.store
            .list_skill_resolution_rates(&request.tenant_id, request.intent_label.as_deref())
            .await
            .map_err(HandlerError::from)
    }

    async fn refresh_segment_materialized_views_inner(&self) -> Result<(), HandlerError> {
        self.store
            .refresh_segment_materialized_views()
            .await
            .map_err(HandlerError::from)
    }

    async fn record_segment_tool_use_inner(
        &self,
        request: RecordSegmentToolUseRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_tool_use(request.session_id, &request.tool_name)
            .await
            .map_err(HandlerError::from)
    }

    async fn record_segment_skill_activation_inner(
        &self,
        request: RecordSegmentSkillActivationRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_skill_activation(request.session_id, &request.skill_name)
            .await
            .map_err(HandlerError::from)
    }

    async fn record_segment_turn_usage_inner(
        &self,
        request: RecordSegmentTurnUsageRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_turn_usage(request.session_id, request.token_cost)
            .await
            .map_err(HandlerError::from)
    }
}

impl SessionStore for SessionStoreImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    async fn create_session(
        &self,
        ctx: Context<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<Json<SessionId>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "create_session");
        let store = self.store.clone();
        let meta = meta.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.create_session_inner(meta).await.map(Json::from) })
            .name("create_session")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn append_event(
        &self,
        ctx: Context<'_>,
        request: Json<AppendEventRequest>,
    ) -> Result<u64, HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_event");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.append_event_inner(request).await })
            .name("append_event")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn get_events(
        &self,
        ctx: Context<'_>,
        request: Json<GetEventsRequest>,
    ) -> Result<Json<Vec<EventRecord>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_events");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.get_events_inner(request).await.map(Json::from) })
            .name("get_events")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, session_id))]
    async fn get_session(
        &self,
        ctx: Context<'_>,
        session_id: Json<SessionId>,
    ) -> Result<Json<SessionMeta>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_session");
        let store = self.store.clone();
        let session_id = session_id.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.get_session_inner(session_id).await.map(Json::from) })
            .name("get_session")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn update_status(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateStatusRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_status");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.update_status_inner(request).await })
            .name("update_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn search_events(
        &self,
        ctx: Context<'_>,
        request: Json<SearchEventsRequest>,
    ) -> Result<Json<Vec<EventRecord>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "search_events");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.search_events_inner(request).await.map(Json::from) })
            .name("search_events")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn init_session_vo(
        &self,
        ctx: Context<'_>,
        request: Json<InitSessionVoRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "init_session_vo");
        let request = request.into_inner();
        ctx.object_client::<SessionClient>(request.session_id.to_string())
            .set_meta(Json::from(request.meta))
            .call()
            .await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn create_segment(
        &self,
        ctx: Context<'_>,
        request: Json<CreateSegmentRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "create_segment");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.create_segment_inner(request).await })
            .name("create_segment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn complete_segment(
        &self,
        ctx: Context<'_>,
        request: Json<CompleteSegmentRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "complete_segment");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.complete_segment_inner(request).await })
            .name("complete_segment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, session_id))]
    async fn get_active_segment(
        &self,
        ctx: Context<'_>,
        session_id: Json<SessionId>,
    ) -> Result<Json<Option<TaskSegment>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_active_segment");
        let store = self.store.clone();
        let session_id = session_id.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .get_active_segment_inner(session_id)
                    .await
                    .map(Json::from)
            })
            .name("get_active_segment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, session_id))]
    async fn list_segments(
        &self,
        ctx: Context<'_>,
        session_id: Json<SessionId>,
    ) -> Result<Json<Vec<TaskSegment>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_segments");
        let store = self.store.clone();
        let session_id = session_id.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .list_segments_inner(session_id)
                    .await
                    .map(Json::from)
            })
            .name("list_segments")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn update_segment_resolution(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateSegmentResolutionRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_segment_resolution");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.update_segment_resolution_inner(request).await })
            .name("update_segment_resolution")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn update_segment_resolution_score(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateSegmentResolutionScoreRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_segment_resolution_score");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.update_segment_resolution_score_inner(request).await })
            .name("update_segment_resolution_score")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn get_segment_baseline(
        &self,
        ctx: Context<'_>,
        request: Json<GetSegmentBaselineRequest>,
    ) -> Result<Json<Option<SegmentBaseline>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_segment_baseline");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .get_segment_baseline_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("get_segment_baseline")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_skill_resolution_rates(
        &self,
        ctx: Context<'_>,
        request: Json<ListSkillResolutionRatesRequest>,
    ) -> Result<Json<Vec<SkillResolutionRate>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_skill_resolution_rates");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .list_skill_resolution_rates_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("list_skill_resolution_rates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn refresh_segment_materialized_views(
        &self,
        ctx: Context<'_>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "refresh_segment_materialized_views");
        let store = self.store.clone();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.refresh_segment_materialized_views_inner().await })
            .name("refresh_segment_materialized_views")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn record_segment_tool_use(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentToolUseRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "record_segment_tool_use");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.record_segment_tool_use_inner(request).await })
            .name("record_segment_tool_use")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn record_segment_skill_activation(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentSkillActivationRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "record_segment_skill_activation");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.record_segment_skill_activation_inner(request).await })
            .name("record_segment_skill_activation")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn record_segment_turn_usage(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentTurnUsageRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "record_segment_turn_usage");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.record_segment_turn_usage_inner(request).await })
            .name("record_segment_turn_usage")
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::{Result, anyhow};
    use moa_core::{
        Event, EventFilter, EventRange, ModelId, SessionMeta, SessionStatus, UserId, WorkspaceId,
    };
    use moa_session::testing;
    use restate_sdk::prelude::HandlerError;

    use super::{
        AppendEventRequest, GetEventsRequest, SearchEventsRequest, SessionStoreImpl,
        UpdateStatusRequest,
    };

    fn test_session_meta(workspace_id: &str) -> SessionMeta {
        SessionMeta {
            workspace_id: WorkspaceId::new(workspace_id),
            user_id: UserId::new("user-1"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        }
    }

    async fn test_service() -> Result<(SessionStoreImpl, String, String)> {
        let (store, database_url, schema_name) = testing::create_isolated_test_store().await?;
        Ok((
            SessionStoreImpl::new(Arc::new(store)),
            database_url,
            schema_name,
        ))
    }

    async fn cleanup(database_url: &str, schema_name: &str) -> Result<()> {
        testing::cleanup_test_schema(database_url, schema_name).await?;
        Ok(())
    }

    fn into_anyhow(error: HandlerError) -> anyhow::Error {
        anyhow!("{error:?}")
    }

    #[tokio::test]
    async fn append_event_increments_sequence() -> Result<()> {
        let (service, database_url, schema_name) = test_service().await?;
        let session_id = service
            .create_session_inner(test_session_meta("append-seq"))
            .await
            .map_err(into_anyhow)?;

        let seq0 = service
            .append_event_inner(AppendEventRequest {
                session_id,
                event: Event::UserMessage {
                    text: "first".to_string(),
                    attachments: vec![],
                },
            })
            .await
            .map_err(into_anyhow)?;
        let seq1 = service
            .append_event_inner(AppendEventRequest {
                session_id,
                event: Event::UserMessage {
                    text: "second".to_string(),
                    attachments: vec![],
                },
            })
            .await
            .map_err(into_anyhow)?;
        let seq2 = service
            .append_event_inner(AppendEventRequest {
                session_id,
                event: Event::UserMessage {
                    text: "third".to_string(),
                    attachments: vec![],
                },
            })
            .await
            .map_err(into_anyhow)?;

        assert_eq!((seq0, seq1, seq2), (0, 1, 2));

        cleanup(&database_url, &schema_name).await
    }

    #[tokio::test]
    async fn get_events_respects_range() -> Result<()> {
        let (service, database_url, schema_name) = test_service().await?;
        let session_id = service
            .create_session_inner(test_session_meta("range"))
            .await
            .map_err(into_anyhow)?;

        for index in 0..10 {
            service
                .append_event_inner(AppendEventRequest {
                    session_id,
                    event: Event::UserMessage {
                        text: format!("message {index}"),
                        attachments: vec![],
                    },
                })
                .await
                .map_err(into_anyhow)?;
        }

        let events = service
            .get_events_inner(GetEventsRequest {
                session_id,
                range: EventRange {
                    from_seq: Some(3),
                    to_seq: Some(7),
                    event_types: None,
                    limit: None,
                },
            })
            .await
            .map_err(into_anyhow)?;

        assert_eq!(events.len(), 5);
        assert_eq!(events.first().map(|record| record.sequence_num), Some(3));
        assert_eq!(events.last().map(|record| record.sequence_num), Some(7));

        cleanup(&database_url, &schema_name).await
    }

    #[tokio::test]
    async fn update_status_affects_get_session() -> Result<()> {
        let (service, database_url, schema_name) = test_service().await?;
        let session_id = service
            .create_session_inner(test_session_meta("status"))
            .await
            .map_err(into_anyhow)?;

        service
            .update_status_inner(UpdateStatusRequest {
                session_id,
                status: SessionStatus::Completed,
            })
            .await
            .map_err(into_anyhow)?;
        let session = service
            .get_session_inner(session_id)
            .await
            .map_err(into_anyhow)?;

        assert_eq!(session.status, SessionStatus::Completed);
        assert!(session.completed_at.is_some());

        cleanup(&database_url, &schema_name).await
    }

    #[tokio::test]
    async fn search_events_finds_by_payload() -> Result<()> {
        let (service, database_url, schema_name) = test_service().await?;
        let session_id = service
            .create_session_inner(test_session_meta("search"))
            .await
            .map_err(into_anyhow)?;

        service
            .append_event_inner(AppendEventRequest {
                session_id,
                event: Event::UserMessage {
                    text: "Fix the OAuth refresh token bug".to_string(),
                    attachments: vec![],
                },
            })
            .await
            .map_err(into_anyhow)?;
        service
            .append_event_inner(AppendEventRequest {
                session_id,
                event: Event::UserMessage {
                    text: "Debug the refresh-token rotation failure".to_string(),
                    attachments: vec![],
                },
            })
            .await
            .map_err(into_anyhow)?;

        let events = service
            .search_events_inner(SearchEventsRequest {
                query: "refresh-token".to_string(),
                filter: EventFilter::default(),
            })
            .await
            .map_err(into_anyhow)?;

        assert!(events.iter().any(|record| matches!(
            &record.event,
            Event::UserMessage { text, .. } if text.contains("refresh-token")
        )));

        cleanup(&database_url, &schema_name).await
    }
}
