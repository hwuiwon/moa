//! Restate handlers for the session-store facade.

use super::inner::{create_agent_session_for_identity, create_session_for_identity};
use super::*;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use moa_authz::{
    AuthzCheckError, OutboxPoller, PollerConfig, fga_subject, require_authz_with_delegation,
};
use moa_authz_schema::{ObjectType, Relation};
use std::time::Duration;

const SESSION_AUTHZ_VISIBILITY_ATTEMPTS: usize = 6;
const SESSION_AUTHZ_VISIBILITY_DELAY: Duration = Duration::from_millis(25);

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
        let vo_meta = meta.clone();
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

        let create_identity = identity.clone();
        let session_id = ctx
            .run(|| async move {
                create_session_for_identity(store.as_ref(), meta, create_identity)
                    .await
                    .map(Json::from)
            })
            .name("create_session")
            .await?
            .into_inner();
        ensure_session_authz_visible(&ctx, self.store.pool().clone(), fga, &identity, session_id)
            .await?;
        ctx.object_client::<SessionClient>(session_id.to_string())
            .set_meta(Json::from(vo_meta))
            .call()
            .await?;
        Ok(Json::from(session_id))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn create_agent_session(
        &self,
        ctx: Context<'_>,
        request: Json<CreateAgentSessionRequest>,
    ) -> Result<Json<CreateAgentSessionResponse>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "create_agent_session");
        let store = self.store.clone();
        let request = request.into_inner();
        let mut vo_meta = request.meta.clone();
        let identity = require_identity(&ctx)?;
        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Workspace,
            &request.meta.workspace_id,
            Relation::Member,
        )
        .await
        .map_err(translate_authz_error)?;

        let create_identity = identity.clone();
        let response = ctx
            .run(|| async move {
                create_agent_session_for_identity(store.as_ref(), request, create_identity)
                    .await
                    .map(Json::from)
            })
            .name("create_agent_session")
            .await?
            .into_inner();
        ensure_session_authz_visible(
            &ctx,
            self.store.pool().clone(),
            fga,
            &identity,
            response.session_id,
        )
        .await?;
        vo_meta.agent_context = Some(response.agent_context.clone());
        ctx.object_client::<SessionClient>(response.session_id.to_string())
            .set_meta(Json::from(vo_meta))
            .call()
            .await?;
        Ok(Json::from(response))
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
        authorize_session_read(&ctx, request.session_id).await?;
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
        authorize_session_read(&ctx, session_id).await?;
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
    async fn update_segment_assessment(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateSegmentAssessmentRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_segment_assessment");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.update_segment_assessment_inner(request).await })
            .name("update_segment_assessment")
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

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_task_strategy_success_rates(
        &self,
        ctx: Context<'_>,
        request: Json<ListTaskStrategySuccessRatesRequest>,
    ) -> Result<Json<Vec<TaskStrategySuccessRate>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_task_strategy_success_rates");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .list_task_strategy_success_rates_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("list_task_strategy_success_rates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn append_experience_record(
        &self,
        ctx: Context<'_>,
        request: Json<AppendExperienceRecordRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_experience_record");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.append_experience_record_inner(request).await })
            .name("append_experience_record")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_experience_records(
        &self,
        ctx: Context<'_>,
        request: Json<ListExperienceRecordsRequest>,
    ) -> Result<Json<Vec<ExperienceRecord>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_experience_records");
        let store = self.store.clone();
        let request = request.into_inner();
        authorize_session_read(&ctx, request.session_id).await?;
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .list_experience_records_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("list_experience_records")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn append_experience_attributions(
        &self,
        ctx: Context<'_>,
        request: Json<AppendExperienceAttributionsRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_experience_attributions");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.append_experience_attributions_inner(request).await })
            .name("append_experience_attributions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_experience_attributions(
        &self,
        ctx: Context<'_>,
        request: Json<ListExperienceAttributionsRequest>,
    ) -> Result<Json<Vec<ExperienceAttribution>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_experience_attributions");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .list_experience_attributions_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("list_experience_attributions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn append_learning_candidate(
        &self,
        ctx: Context<'_>,
        request: Json<AppendLearningCandidateRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_learning_candidate");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move { service.append_learning_candidate_inner(request).await })
            .name("append_learning_candidate")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn get_learning_candidate(
        &self,
        ctx: Context<'_>,
        request: Json<GetLearningCandidateRequest>,
    ) -> Result<Json<LearningCandidate>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_learning_candidate");
        let store = self.store.clone();
        let request = request.into_inner();
        authorize_workspace_read(&ctx, &request.workspace_id).await?;
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .get_learning_candidate_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("get_learning_candidate")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_learning_candidates(
        &self,
        ctx: Context<'_>,
        request: Json<ListLearningCandidatesRequest>,
    ) -> Result<Json<Vec<LearningCandidate>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_learning_candidates");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .list_learning_candidates_inner(request)
                    .await
                    .map(Json::from)
            })
            .name("list_learning_candidates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn update_learning_candidate_status(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateLearningCandidateStatusRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_learning_candidate_status");
        let store = self.store.clone();
        let request = request.into_inner();
        let service = Self { store };

        Ok(ctx
            .run(|| async move {
                service
                    .update_learning_candidate_status_inner(request)
                    .await
            })
            .name("update_learning_candidate_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    async fn refresh_segment_materialized_views(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
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

async fn authorize_session_read(
    ctx: &impl RequestHeaders,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)
}

async fn authorize_workspace_read(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        Relation::Member,
    )
    .await
    .map_err(translate_authz_error)
}

async fn ensure_session_authz_visible(
    ctx: &Context<'_>,
    pool: sqlx::PgPool,
    fga: moa_authz::FgaClient,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let subject = fga_subject(identity);
    let object = format!("{}:{session_id}", ObjectType::Session);
    for attempt in 0..SESSION_AUTHZ_VISIBILITY_ATTEMPTS {
        let visible = fga
            .check(&subject, &Relation::Participant.to_string(), &object)
            .await
            .map_err(|error| translate_authz_error(AuthzCheckError::Engine(error)))?;
        if visible {
            return Ok(());
        }

        let poller = OutboxPoller::new(
            pool.clone(),
            fga.clone(),
            PollerConfig {
                batch_size: 128,
                ..PollerConfig::default()
            },
        );
        ctx.run(move || async move {
            poller.tick().await.map_err(|error| {
                HandlerError::from(TerminalError::new(format!(
                    "authz outbox visibility drain: {error}"
                )))
            })?;
            Ok::<(), HandlerError>(())
        })
        .name(format!("create_session_authz_visibility_{attempt}"))
        .await?;

        if attempt + 1 < SESSION_AUTHZ_VISIBILITY_ATTEMPTS {
            tokio::time::sleep(SESSION_AUTHZ_VISIBILITY_DELAY).await;
        }
    }

    Err(translate_authz_error(AuthzCheckError::Forbidden {
        subject,
        object_type: ObjectType::Session,
        object_id: session_id.to_string(),
        relation: Relation::Participant,
    }))
}
