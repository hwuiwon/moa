//! Restate handlers for the session-store facade.

use super::*;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};

impl RestateSessionStore for SessionStoreImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    async fn create_session(
        &self,
        ctx: Context<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<Json<SessionId>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "create_session");
        let store = self.store.clone();
        let meta = meta.into_inner();
        let identity = require_identity(&ctx)?;
        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Workspace,
            &meta.workspace_id,
            Relation::Member,
        )
        .await
        .map_err(translate_authz_error)?;
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .create_session_authorized_inner(meta, identity)
                    .await
                    .map(Json::from)
            })
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
    async fn list_sessions(
        &self,
        ctx: Context<'_>,
        request: Json<ListSessionsRequest>,
    ) -> Result<Json<Vec<SessionSummary>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_sessions");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.list_sessions_inner(request).await.map(Json::from) })
            .name("list_sessions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn workspace_cost_since(
        &self,
        ctx: Context<'_>,
        request: Json<WorkspaceCostSinceRequest>,
    ) -> Result<u32, HandlerError> {
        annotate_restate_handler_span("SessionStore", "workspace_cost_since");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.workspace_cost_since_inner(request).await })
            .name("workspace_cost_since")
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
