//! Restate handlers for the session-store facade.

use std::time::Instant;

use super::inner::{create_agent_session_for_identity, create_session_for_identity};
use super::*;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{AuthzEnforcer, require_identity, translate_authz_error};
use crate::workflows::session_retention::{
    SessionRetentionClient, SessionRetentionDispatch, SessionRetentionRequest,
    session_retention_workflow_id,
};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};

impl RestateSessionStore for SessionStoreImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    async fn create_session(
        &self,
        ctx: Context<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<Json<SessionId>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "create_session");
        let store = self.store.clone();
        let pool = self.pool.clone();
        let meta = meta.into_inner();
        let vo_meta = meta.clone();
        let identity = require_identity(&ctx)?;
        let fga = self.authz.require_fga_client()?;
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
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .set_meta(Json::from(vo_meta)),
        )
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "create_agent_session");
        let store = self.store.clone();
        let pool = self.pool.clone();
        let request = request.into_inner();
        let mut vo_meta = request.meta.clone();
        let identity = require_identity(&ctx)?;
        let fga = self.authz.require_fga_client()?;
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
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(response.session_id.to_string())
                .set_meta(Json::from(vo_meta)),
        )
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
    ) -> Result<Json<EventRecord>, HandlerError> {
        let handler_started = Instant::now();
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "append_event");
        let request = request.into_inner();
        let store = self.store.clone();

        let action_started = Instant::now();
        let result = ctx
            .run(|| async move {
                if matches!(&request.event, Event::Error { .. }) {
                    record_session_error("event_log");
                }
                store
                    .emit_event_record(request.session_id, request.event, request.dedupe_key)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("append_event")
            .await;
        moa_observability::record_session_event_append_phase_duration(
            moa_observability::SessionEventAppendPhase::HandlerAction,
            action_started.elapsed(),
        );
        moa_observability::record_session_event_append_phase_duration(
            moa_observability::SessionEventAppendPhase::HandlerTotal,
            handler_started.elapsed(),
        );
        Ok(result?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn get_events(
        &self,
        ctx: Context<'_>,
        request: Json<GetEventsRequest>,
    ) -> Result<Json<Vec<EventRecord>>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "get_events");
        let request = request.into_inner();
        authorize_session_read(&self.authz, &ctx, request.session_id).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "get_session");
        let session_id = session_id.into_inner();
        authorize_session_read(&self.authz, &ctx, session_id).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "search_events");
        let request = request.into_inner();
        authorize_event_search(&self.authz, &ctx, &request).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "list_sessions");
        let request = request.into_inner();
        let tenant_id = tenant_id_for_session_listing(&request)?;
        authorize_tenant_admin(&self.authz, &ctx, tenant_id).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "tenant_cost_since");
        let request = request.into_inner();
        authorize_tenant_read(&self.authz, &ctx, request.tenant_id).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "init_session_vo");
        let request = request.into_inner();
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(request.session_id.to_string())
                .set_meta(Json::from(request.meta)),
        )
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "list_experience_records");
        let request = request.into_inner();
        authorize_session_read(&self.authz, &ctx, request.session_id).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "get_learning_candidate");
        let request = request.into_inner();
        authorize_tenant_read(&self.authz, &ctx, request.tenant_id).await?;
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "list_learning_candidates");
        let request = request.into_inner();
        authorize_tenant_read(&self.authz, &ctx, request.tenant_id).await?;
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
    async fn start_session_retention(
        &self,
        ctx: Context<'_>,
        request: Json<SessionRetentionRequest>,
    ) -> Result<Json<SessionRetentionDispatch>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "start_session_retention");
        let request = request.into_inner();
        let identity = require_identity(&ctx)?;
        let fga = self.authz.require_fga_client()?;
        // Retention deletes a tenant's live conversation history. Tenant
        // operator is not enough: this is the same class of irreversible act as
        // a purge, so it requires tenant admin on the tenant being retained.
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            request.tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error)?;

        let target_date =
            ctx.run(|| async move {
                Ok::<_, HandlerError>(Json::from(chrono::Utc::now().date_naive()))
            })
            .name("session-retention-date")
            .await?
            .into_inner();
        // One pass per tenant per logical day: a retried dispatch lands on the
        // same durable workflow instead of starting a second concurrent pass
        // over the same candidates.
        let workflow_id = session_retention_workflow_id(&request.tenant_id, target_date);
        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<SessionRetentionClient>(workflow_id.clone())
                .run(Json::from(request)),
        )
        .send();
        Ok(Json::from(SessionRetentionDispatch {
            workflow_id,
            target_date,
        }))
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal maintenance handler refreshes derived materialized views only.
    async fn refresh_segment_materialized_views(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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

    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal maintenance handler; reads derived learning aggregates and files
    // tenant-scoped rollback proposals into the human review queue. No caller data is returned.
    async fn monitor_skill_regressions(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "monitor_skill_regressions");
        let store = self.store.clone();
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let filed = moa_skills::rollback::monitor_and_file_skill_regressions(
                    &store,
                    &config.learning.regression_monitor,
                    chrono::Utc::now(),
                )
                .await
                .map_err(HandlerError::from)?;
                // Recorded inside the durable step so a replay reuses the journaled
                // result instead of re-incrementing the counter.
                moa_observability::runtime_metrics::record_skill_learning_candidates_filed(
                    "rollback_monitor",
                    "regression",
                    filed as u64,
                );
                Ok::<(), HandlerError>(())
            })
            .name("monitor_skill_regressions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal maintenance handler; embeds tenant-owned task summaries and
    // serving skill identities into derived vector columns. No caller data is returned.
    async fn backfill_learning_embeddings(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "backfill_learning_embeddings");
        let Some(embedder) = self.embedder.clone() else {
            // No embedder configured: the deployment runs without learning
            // embeddings. Nothing to backfill, so this is a clean no-op.
            tracing::debug!("learning-embeddings backfill skipped: embedder disabled");
            return Ok(());
        };
        let store = self.store.clone();
        let registry = moa_artifacts::registry::ArtifactRegistry::new(self.pool.clone());
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let embedder_ref = embedder.as_ref();
                // Task summaries are transcript-derived, and this is an automatic
                // provider call over stored rows, so each one is sanitized before
                // it is embedded. The deterministic heuristic keeps the durable
                // step free of network IO, and matches what the filing-time probe
                // sanitizes with so both land in one vector space.
                moa_skills::embeddings::backfill_experience_embeddings(
                    &store,
                    embedder_ref,
                    &moa_memory_pii::HeuristicPiiClassifier,
                    &config.learning.embeddings,
                    chrono::Utc::now(),
                )
                .await
                .map_err(HandlerError::from)?;
                moa_skills::embeddings::backfill_skill_embeddings(
                    &registry,
                    embedder_ref,
                    &config.learning.embeddings,
                )
                .await
                .map_err(HandlerError::from)?;
                Ok::<(), HandlerError>(())
            })
            .name("backfill_learning_embeddings")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal maintenance handler; reads derived tenant-owned learning
    // aggregates and dispatches tenant-scoped skill-learning workflows into the
    // human review pipeline. No caller data is returned.
    async fn mine_task_recurrences(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "mine_task_recurrences");
        let store = self.store.clone();
        let config = self.config.clone();

        // Discovery + qualification runs in one durable step so the dispatch set
        // is journaled: a replay re-dispatches the exact same exemplars instead of
        // re-reading a changed ledger.
        let dispatches = ctx
            .run(|| async move {
                discover_recurrence_dispatches(
                    &store,
                    &config.learning.recurrence,
                    chrono::Utc::now(),
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
            })
            .name("mine_task_recurrences")
            .await?
            .into_inner();

        // Each SkillLearning workflow is keyed by its exemplar experience id, so a
        // re-dispatch of the same exemplar attaches to the existing run rather than
        // filing twice; the open-candidate/cooldown suppression above keeps a later
        // tick from re-qualifying a fingerprint whose proposal is already filed.
        let dispatched = dispatches.len();
        for request in dispatches {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<crate::workflows::skill_learning::SkillLearningClient>(
                    request.experience_id.to_string(),
                )
                .run(Json::from(request)),
            )
            .send();
        }
        tracing::info!(
            dispatched,
            "recurrence mining dispatched skill-learning passes"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning telemetry write emitted by admitted session workflows.
    async fn record_segment_tool_use(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentToolUseRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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
    async fn record_segment_skill_use(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentSkillUseRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionStore", "record_segment_skill_use");
        let request = request.into_inner();
        let store = self.store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .record_active_segment_skill_use(request.session_id, &request.skill_name)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("record_segment_skill_use")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal learning telemetry write emitted by admitted session workflows.
    async fn record_segment_turn_usage(
        &self,
        ctx: Context<'_>,
        request: Json<RecordSegmentTurnUsageRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
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

/// Discovers the recurrence-triggered skill-learning dispatches for one tick.
///
/// Store-coupled driver over the pure qualifier: for each tenant with recent
/// resolved/partial experiences, it groups them into recurring fingerprint
/// clusters, consults the fingerprint's candidate decision history for
/// suppression, and turns each qualifying cluster into a keyed
/// [`RunSkillLearningRequest`] whose exemplar carries the relaxed floor and whose
/// siblings ride along for immediate generalization. Pure ranking/suppression
/// stays in `moa-skills`; only the reads happen here.
async fn discover_recurrence_dispatches(
    store: &moa_session::PostgresSessionStore,
    config: &moa_config::RecurrenceConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<crate::workflows::skill_learning::RunSkillLearningRequest>, moa_core::error::MoaError>
{
    use crate::workflows::skill_learning::{
        RecurrenceDispatch, RecurrenceSiblingRef, RunSkillLearningRequest,
    };
    use std::collections::HashMap;

    use moa_skills::recurrence::{
        RecurrenceThresholds, cluster_recurrence_groups, qualify_recurrence_cluster,
    };

    let thresholds = RecurrenceThresholds::from_config(config);
    let since = now - chrono::Duration::days(config.lookback_days.max(0));
    let tenants = store
        .list_tenants_with_recent_learnable_experiences(since)
        .await?;

    let mut dispatches = Vec::new();
    for tenant_id in tenants {
        // Load every candidate group down to a single occurrence (bounded by
        // recency): the occurrence threshold is applied after clustering by the
        // pure qualifier, so sub-threshold aliases can still merge into a
        // qualifying cluster.
        let groups = store
            .list_candidate_experience_groups(&tenant_id, since, config.max_candidate_groups)
            .await?;

        // Probe one representative per group against the tenant's task embeddings
        // so semantically-equal groups ("same loop, different wording") merge into
        // one cluster. The breadth is bounded by the total grouped members so a
        // representative can reach every other group's members; an unembedded
        // representative yields None and its group stays exact (NULL degradation).
        let neighbor_limit = recurrence_cluster_neighbor_limit(&groups);
        let mut neighbor_lists = Vec::with_capacity(groups.len());
        for group in &groups {
            let representative = group.members.first().map(|member| member.experience_id);
            let neighbors = match representative {
                Some(experience_id) => {
                    store
                        .nearest_task_embeddings_for_experience(
                            &tenant_id,
                            experience_id,
                            neighbor_limit,
                        )
                        .await?
                }
                None => None,
            };
            neighbor_lists.push(neighbors);
        }
        let clusters =
            cluster_recurrence_groups(&groups, &neighbor_lists, config.cluster_similarity);

        // Batch the per-cluster suppression history: one `= ANY(...)` scan over
        // every fingerprint across every cluster replaces the per-fingerprint N+1,
        // then each cluster reads its merged fingerprints' decisions from the map.
        let all_fingerprints: Vec<String> = clusters
            .iter()
            .flat_map(|cluster| cluster.merged_fingerprints.iter().cloned())
            .collect();
        let mut decisions_by_fingerprint: HashMap<String, Vec<_>> = HashMap::new();
        for (fingerprint_hash, decision) in store
            .list_skill_candidate_decisions_for_fingerprints(&tenant_id, &all_fingerprints)
            .await?
        {
            decisions_by_fingerprint
                .entry(fingerprint_hash)
                .or_default()
                .push(decision);
        }

        for cluster in clusters {
            // Per-cluster suppression: gather the candidate history across every
            // merged fingerprint so an open/promoted/cooldown candidate on any one
            // member suppresses the whole cluster.
            let decisions: Vec<_> = cluster
                .merged_fingerprints
                .iter()
                .filter_map(|hash| decisions_by_fingerprint.get(hash))
                .flatten()
                .cloned()
                .collect();
            let Some(plan) = qualify_recurrence_cluster(&cluster, &decisions, &thresholds, now)
            else {
                continue;
            };
            let siblings = plan
                .siblings
                .iter()
                .map(|sibling| RecurrenceSiblingRef {
                    session_id: sibling.session_id,
                    experience_id: sibling.experience_id,
                })
                .collect();
            dispatches.push(RunSkillLearningRequest {
                session_id: plan.exemplar.session_id,
                experience_id: plan.exemplar.experience_id,
                recurrence: Some(RecurrenceDispatch {
                    occurrences: plan.occurrences,
                    merged_fingerprints: plan.merged_fingerprints,
                    first_seen: plan.first_seen,
                    last_seen: plan.last_seen,
                    siblings,
                }),
            });
        }
    }
    Ok(dispatches)
}

/// Upper bound on the per-representative neighbor breadth used for recurrence
/// clustering, so a pathological tenant never triggers an unbounded scan.
const RECURRENCE_CLUSTER_NEIGHBOR_LIMIT_MAX: usize = 200;

/// Neighbor breadth to request per group representative when clustering.
///
/// A representative must be able to see every other grouped member to discover a
/// merge (within-group members saturate the closest ranks), so the breadth tracks
/// the total grouped members, clamped to at least one and at most
/// [`RECURRENCE_CLUSTER_NEIGHBOR_LIMIT_MAX`]. A clamp-induced miss only fails to
/// merge, degrading safely to exact grouping.
fn recurrence_cluster_neighbor_limit(groups: &[moa_session::RecurringExperienceCluster]) -> usize {
    let total_members: usize = groups.iter().map(|group| group.members.len()).sum();
    total_members.clamp(1, RECURRENCE_CLUSTER_NEIGHBOR_LIMIT_MAX)
}

async fn authorize_session_read(
    authz: &AuthzEnforcer,
    ctx: &impl RequestHeaders,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = authz.require_fga_client()?;
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
    authz: &AuthzEnforcer,
    ctx: &impl RequestHeaders,
    request: &SearchEventsRequest,
) -> Result<(), HandlerError> {
    if let Some(session_id) = request.filter.session_id {
        return authorize_session_read(authz, ctx, session_id).await;
    }

    let tenant_id = request.filter.tenant_id.ok_or_else(|| {
        TerminalError::new_with_code(400, "search_events requires session_id or tenant_id")
    })?;
    authorize_tenant_admin(authz, ctx, tenant_id).await
}

async fn authorize_tenant_read(
    authz: &AuthzEnforcer,
    ctx: &impl RequestHeaders,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<(), HandlerError> {
    authz
        .authorize_tenant(ctx, tenant_id, Relation::Operator)
        .await?;
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
    authz: &AuthzEnforcer,
    ctx: &impl RequestHeaders,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<(), HandlerError> {
    authz
        .authorize_tenant(ctx, tenant_id, Relation::Admin)
        .await?;
    Ok(())
}

async fn ensure_session_authz_visible(
    ctx: &Context<'_>,
    pool: sqlx::PgPool,
    fga: moa_authz::FgaClient,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let identity = identity.clone();
    ctx.run(move || async move {
        super::inner::ensure_session_authz_visible(&pool, &fga, &identity, session_id).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("create_session_authz_visibility")
    .await?;
    Ok(())
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
