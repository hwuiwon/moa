//! Restate handlers for the session-store facade.

use super::inner::{create_agent_session_for_identity, create_session_for_identity};
use super::*;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{
    authorize_tenant, require_fga_client, require_identity, translate_authz_error,
};
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
        let pool = self.pool.clone();
        let meta = meta.into_inner();
        let vo_meta = meta.clone();
        let identity = require_identity(&ctx)?;
        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            meta.tenant_id,
            Relation::Operator,
        )
        .await
        .map_err(translate_authz_error)?;

        let create_identity = identity.clone();
        let session_id = ctx
            .run(|| async move {
                create_session_for_identity(store.as_ref(), &pool, meta, create_identity)
                    .await
                    .map(Json::from)
            })
            .name("create_session")
            .await?
            .into_inner();
        ensure_session_authz_visible(&ctx, self.pool.clone(), fga, &identity, session_id).await?;
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
        let pool = self.pool.clone();
        let request = request.into_inner();
        let mut vo_meta = request.meta.clone();
        let identity = require_identity(&ctx)?;
        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            request.meta.tenant_id,
            Relation::Operator,
        )
        .await
        .map_err(translate_authz_error)?;

        let create_identity = identity.clone();
        let response = ctx
            .run(|| async move {
                create_agent_session_for_identity(
                    store.as_ref(),
                    pool.clone(),
                    request,
                    create_identity,
                )
                .await
                .map(Json::from)
            })
            .name("create_agent_session")
            .await?
            .into_inner();
        ensure_session_authz_visible(&ctx, self.pool.clone(), fga, &identity, response.session_id)
            .await?;
        vo_meta.agent_context = Some(response.agent_context.clone());
        ctx.object_client::<SessionClient>(response.session_id.to_string())
            .set_meta(Json::from(vo_meta))
            .call()
            .await?;
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal Restate workflows/services call this via service_client; public edge routes do not expose SessionStore/append_event.
    async fn append_event(
        &self,
        ctx: Context<'_>,
        request: Json<AppendEventRequest>,
    ) -> Result<u64, HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_event");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                if matches!(&request.event, Event::Error { .. }) {
                    record_session_error("event_log");
                }
                store
                    .emit_event_record(request.session_id, request.event, request.dedupe_key)
                    .await
                    .map(|record| record.sequence_num)
                    .map_err(HandlerError::from)
            })
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
        let request = request.into_inner();
        authorize_session_read(&ctx, request.session_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .get_events(request.session_id, request.range)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
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
        let session_id = session_id.into_inner();
        authorize_session_read(&ctx, session_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .get_session(session_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("get_session")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Session VOs sync status via internal service_client calls; public edge routes do not expose SessionStore/update_status.
    async fn update_status(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateStatusRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_status");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .update_status(request.session_id, request.status)
                    .await
                    .map_err(HandlerError::from)
            })
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
        let request = request.into_inner();
        authorize_event_search(&ctx, &request).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .search_events(&request.query, request.filter)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
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
        let request = request.into_inner();
        let tenant_id = tenant_id_for_session_listing(&request)?;
        authorize_tenant_admin(&ctx, tenant_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .list_sessions(request.filter)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_sessions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn tenant_cost_since(
        &self,
        ctx: Context<'_>,
        request: Json<TenantCostSinceRequest>,
    ) -> Result<u32, HandlerError> {
        annotate_restate_handler_span("SessionStore", "tenant_cost_since");
        let request = request.into_inner();
        authorize_tenant_read(&ctx, request.tenant_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .tenant_cost_since(&request.tenant_id, request.since)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("tenant_cost_since")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal session-creation path initializes the VO after create_session/create_agent_session authz.
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
    // SAFETY: Internal Session/TurnExecution workflow call after the owning session has admitted the caller.
    async fn create_segment(
        &self,
        ctx: Context<'_>,
        request: Json<CreateSegmentRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "create_segment");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .create_segment(&request.segment)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("create_segment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal Session/TurnExecution workflow call after the owning session has admitted the caller.
    async fn complete_segment(
        &self,
        ctx: Context<'_>,
        request: Json<CompleteSegmentRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "complete_segment");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .complete_segment(request.segment_id, request.update)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("complete_segment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, session_id))]
    // SAFETY: Internal workflow read used by the owning session; public segment reads authorize through service APIs.
    async fn get_active_segment(
        &self,
        ctx: Context<'_>,
        session_id: Json<SessionId>,
    ) -> Result<Json<Option<TaskSegment>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_active_segment");
        let session_id = session_id.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .get_active_segment(session_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("get_active_segment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, session_id))]
    // SAFETY: Internal workflow read used by the owning session; public segment reads authorize through service APIs.
    async fn list_segments(
        &self,
        ctx: Context<'_>,
        session_id: Json<SessionId>,
    ) -> Result<Json<Vec<TaskSegment>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_segments");
        let session_id = session_id.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .list_segments(session_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_segments")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal assessment write emitted by admitted session workflows.
    async fn update_segment_assessment(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateSegmentAssessmentRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_segment_assessment");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .update_segment_assessment(request.segment_id, &request.assessment)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("update_segment_assessment")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning read used by admitted session workflows.
    async fn get_segment_baseline(
        &self,
        ctx: Context<'_>,
        request: Json<GetSegmentBaselineRequest>,
    ) -> Result<Json<Option<SegmentBaseline>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "get_segment_baseline");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                let tenant_id = request.tenant_id.to_string();
                store
                    .get_segment_baseline(&tenant_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("get_segment_baseline")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal skill-learning read used by admitted session workflows.
    async fn list_skill_resolution_rates(
        &self,
        ctx: Context<'_>,
        request: Json<ListSkillResolutionRatesRequest>,
    ) -> Result<Json<Vec<SkillResolutionRate>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_skill_resolution_rates");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                let tenant_id = request.tenant_id.to_string();
                store
                    .list_skill_resolution_rates(&tenant_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_skill_resolution_rates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal strategy-learning read used by admitted session workflows.
    async fn list_task_strategy_success_rates(
        &self,
        ctx: Context<'_>,
        request: Json<ListTaskStrategySuccessRatesRequest>,
    ) -> Result<Json<Vec<TaskStrategySuccessRate>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_task_strategy_success_rates");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                let tenant_id = request.tenant_id.to_string();
                store
                    .list_task_strategy_success_rates(&tenant_id, &request.task_fingerprint)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_task_strategy_success_rates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning write emitted by admitted session workflows.
    async fn append_experience_record(
        &self,
        ctx: Context<'_>,
        request: Json<AppendExperienceRecordRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_experience_record");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .append_experience_record(&request.experience)
                    .await
                    .map_err(HandlerError::from)
            })
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
        let request = request.into_inner();
        authorize_session_read(&ctx, request.session_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .list_experience_records(request.session_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_experience_records")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning write emitted by admitted session workflows.
    async fn append_experience_attributions(
        &self,
        ctx: Context<'_>,
        request: Json<AppendExperienceAttributionsRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_experience_attributions");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .append_experience_attributions(&request.attributions)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("append_experience_attributions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning read used by admitted review and learning workflows.
    async fn list_experience_attributions(
        &self,
        ctx: Context<'_>,
        request: Json<ListExperienceAttributionsRequest>,
    ) -> Result<Json<Vec<ExperienceAttribution>>, HandlerError> {
        annotate_restate_handler_span("SessionStore", "list_experience_attributions");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .list_experience_attributions(request.experience_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_experience_attributions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning write emitted by admitted session workflows.
    async fn append_learning_candidate(
        &self,
        ctx: Context<'_>,
        request: Json<AppendLearningCandidateRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "append_learning_candidate");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .append_learning_candidate(&request.candidate)
                    .await
                    .map_err(HandlerError::from)
            })
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
        let request = request.into_inner();
        authorize_tenant_read(&ctx, request.tenant_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .get_learning_candidate(&request.tenant_id, request.candidate_id)
                    .await
                    .map_err(HandlerError::from)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(
                            404,
                            format!(
                                "learning candidate {} not found in tenant {}",
                                request.candidate_id, request.tenant_id
                            ),
                        )
                        .into()
                    })
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
        let request = request.into_inner();
        authorize_tenant_read(&ctx, request.tenant_id).await?;
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                let tenant_id = request.tenant_id.to_string();
                store
                    .list_learning_candidates(&tenant_id, request.status, request.limit)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_learning_candidates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal review workflow write after LearningReview authorizes candidate access.
    async fn update_learning_candidate_status(
        &self,
        ctx: Context<'_>,
        request: Json<UpdateLearningCandidateStatusRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "update_learning_candidate_status");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .update_learning_candidate_status(&request.update)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("update_learning_candidate_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal maintenance handler refreshes derived materialized views only.
    async fn refresh_segment_materialized_views(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "refresh_segment_materialized_views");
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .refresh_segment_materialized_views()
                    .await
                    .map_err(HandlerError::from)
            })
            .name("refresh_segment_materialized_views")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal maintenance handler refreshes derived materialized views only.
    async fn refresh_analytics_materialized_views(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "refresh_analytics_materialized_views");
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .refresh_analytics_materialized_views()
                    .await
                    .map_err(HandlerError::from)
            })
            .name("refresh_analytics_materialized_views")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning telemetry write emitted by admitted session workflows.
    async fn record_segment_tool_use(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentToolUseRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "record_segment_tool_use");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .record_active_segment_tool_use(request.session_id, &request.tool_name)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("record_segment_tool_use")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning telemetry write emitted by admitted session workflows.
    async fn record_segment_skill_activation(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentSkillActivationRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "record_segment_skill_activation");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .record_active_segment_skill_activation(request.session_id, &request.skill_name)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("record_segment_skill_activation")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning telemetry write emitted by admitted session workflows.
    async fn record_segment_turn_usage(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentTurnUsageRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SessionStore", "record_segment_turn_usage");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .record_active_segment_turn_usage(request.session_id, request.token_cost)
                    .await
                    .map_err(HandlerError::from)
            })
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

async fn authorize_event_search(
    ctx: &impl RequestHeaders,
    request: &SearchEventsRequest,
) -> Result<(), HandlerError> {
    if let Some(session_id) = request.filter.session_id {
        return authorize_session_read(ctx, session_id).await;
    }

    let tenant_id = request.filter.tenant_id.ok_or_else(|| {
        TerminalError::new_with_code(400, "search_events requires session_id or tenant_id")
    })?;
    authorize_tenant_admin(ctx, tenant_id).await
}

async fn authorize_tenant_read(
    ctx: &impl RequestHeaders,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<(), HandlerError> {
    authorize_tenant(ctx, tenant_id, Relation::Operator).await?;
    Ok(())
}

fn tenant_id_for_session_listing(
    request: &ListSessionsRequest,
) -> Result<moa_core::types::identifiers::TenantId, HandlerError> {
    request
        .filter
        .tenant_id
        .ok_or_else(|| TerminalError::new_with_code(400, "list_sessions requires tenant_id").into())
}

async fn authorize_tenant_admin(
    ctx: &impl RequestHeaders,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<(), HandlerError> {
    authorize_tenant(ctx, tenant_id, Relation::Admin).await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::{types::identifiers::TenantId, types::session::SessionFilter};

    #[test]
    fn list_sessions_requires_explicit_tenant_id() {
        // Pins: session listing never falls back to an unscoped cross-tenant read.
        let request = ListSessionsRequest {
            filter: SessionFilter::default(),
        };

        assert!(tenant_id_for_session_listing(&request).is_err());
    }

    #[test]
    fn list_sessions_uses_request_tenant_for_authorization() {
        // Pins: tenant-wide contact-session inspection is authorized on the requested tenant.
        let tenant_id = TenantId::new();
        let request = ListSessionsRequest {
            filter: SessionFilter {
                tenant_id: Some(tenant_id),
                ..SessionFilter::default()
            },
        };

        assert_eq!(
            tenant_id_for_session_listing(&request).expect("tenant id should be accepted"),
            tenant_id
        );
    }
}
