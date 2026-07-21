//! Restate service for compiler-ready execution capabilities.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionBudgetLimit, ExecutionPlanDefinition,
};
use moa_artifacts::execution_plan::{
    ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage,
    PlanAmendment, PlanAmendmentOperation,
};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::skill::{SkillActionDefinition, SkillActionKind};
use moa_authz_schema::Relation;
use moa_core::types::memory::RlsContext;
use moa_core::types::tools::{ToolDefinition, ToolPolicySpec};
use moa_core::{
    config::ExecutionConfig,
    error::{MoaError, Result as MoaResult},
    events::Event,
    traits::SessionStore as _,
    types::action_policy::{ActionPolicyEffect, ActionRuleScope},
    types::agent::AgentSkillPolicy,
    types::events_stream::EventRange,
    types::execution_planning::{
        ExecutionAdmissionEstimate, ExecutionConfirmationEvidence, ExecutionEstimateMethodology,
        ExecutionPlanningContractError, ExecutionRunAdmissionStatus, ExecutionRunStarted,
        ExecutionSourceProvenance,
    },
};
use moa_db::ScopedConn;
use moa_execution::capability::{
    CapabilitiesListRequest, CapabilitiesListResponse, CapabilityCatalogDiagnostic,
    CapabilityCatalogDiagnosticCode, CapabilitySource, ExecutionCapability,
    ExecutionCapabilityCatalog, ExecutionClass, ExecutionEstimate, ExecutionHash, amendment_hash,
    amendment_operations_fingerprint, capability_version, plan_hash,
};
use moa_execution::{
    budget::{BudgetLedger, estimate_fits_limit},
    compiler::{CompileExecutionRequest, ValidateAmendmentRequest, compile, validate_amendment},
    completion::{
        CompletionEvaluationRequest, cancellation_terminal_evidence, evaluate_completion,
        execution_terminal_reason, terminal_evidence_from_evaluation,
    },
    replan::{
        ReplanDecision, ReplanEvaluationRequest, ReplanLoopEvaluationRequest,
        evaluate_replan_loop_stop, evaluate_replan_resource_stop, evaluate_replan_stop,
        failure_fingerprint, replan_stop_gaps, replan_stop_status,
    },
    repository::{
        AmendmentReplayOutcome, AmendmentWrite, CancellationOutcome, CancellationRequest,
        ConfirmationConflict, ConfirmationOutcome, ExecutionRepository, ExecutionRunPageRequest,
        ExecutionRunRecord, ExecutionScope, ExecutionTaskPageRequest, ExecutionTaskRecord,
        NewExecutionPlanningContext, NewExecutionRun, PlanningContextWriteOutcome,
        ReplanStopOutcome, ReplanStopRequest, TaskOutcomeWrite, TransitionOutcome,
        TransitionRejection, ValidatedAmendment,
    },
    schema::validate_instance,
    state::{
        ExecutionRunStatus, ExecutionTaskProjection, ExecutionTaskStatus, ExecutionTerminalCause,
        FailureFingerprintInput,
    },
    wire::{
        ExecutionAmendmentRequest, ExecutionCancelRequest, ExecutionConfirmRequest,
        ExecutionConflictReason, ExecutionInputRequest, ExecutionMutationResponse,
        ExecutionPlanningContextRequest, ExecutionPlanningContextResponse,
        ExecutionPlanningContextSnapshot, ExecutionReviewDecision, ExecutionReviewDecisionRequest,
        ExecutionRunCursor, ExecutionRunListRequest, ExecutionRunListResponse, ExecutionRunRequest,
        ExecutionRunSummary, ExecutionRunWakeReason, ExecutionRunWakeRequest,
        ExecutionSignalRequest, ExecutionStartRequest, ExecutionStartResponse,
        ExecutionStatusResponse, ExecutionSynthesisEvidence, ExecutionSynthesisEvidenceRequest,
        ExecutionTaskCursor, ExecutionTaskListRequest, ExecutionTaskListResponse,
        PinnedExecutionTemplate, PinnedInstructionSkill, decode_cursor, encode_cursor,
        originating_user_event_hash, planning_context_hash,
    },
};
use moa_hands::{ToolExecution, ToolRouter};
use moa_knowledge::repository::{KnowledgeRepository, PostgresKnowledgeRepository};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::handlers::authz_shim::{authorize_session_participant, authorize_tenant};
use crate::objects::session::{ExecutionRunStartedDelivery, SessionClient};
use crate::restate_identity::with_identity_headers;
use crate::workflows::errors::moa_error_to_status_handler_error;
use crate::workflows::execution_node_actions::{
    record_applied_execution_mutation, record_applied_run_transition,
    terminal_projection_from_evaluation,
};
use crate::workflows::execution_run::ExecutionRunClient;
use crate::workflows::execution_task::ExecutionTaskClient;

/// Permanent replay row for one external execution-template admission operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutionTemplateAdmissionRecord {
    /// Stable operation identity reserved before any Session mutation.
    pub operation_uid: uuid::Uuid,
    /// Canonical fingerprint of the complete first request.
    pub request_fingerprint: String,
    /// Exact persisted objective sequence, when committed.
    pub originating_user_sequence_num: Option<u64>,
    /// Exact execution run UID, when committed.
    pub execution_run_uid: Option<uuid::Uuid>,
}

/// Inserts or loads one permanent admission operation under the control-plane RLS scope.
///
/// The Session handler has already authorized the exact parent Session. Control-plane scope is
/// required here so reusing one tenant-scoped caller key with a changed contact can load the first
/// row and report a semantic conflict instead of being hidden by contact RLS.
pub(crate) async fn reserve_execution_template_admission(
    pool: &sqlx::PgPool,
    request: &moa_execution::wire::ExecutionTemplateAdmissionRequest,
    operation_uid: uuid::Uuid,
    request_fingerprint: &str,
) -> moa_core::error::Result<ExecutionTemplateAdmissionRecord> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.execution_template_admission (
            operation_uid,
            tenant_id,
            contact_id,
            session_id,
            idempotency_key,
            request_fingerprint
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(operation_uid)
    .bind(request.tenant_id.0)
    .bind(request.contact_id.map(|contact_id| contact_id.0))
    .bind(request.session_id.0)
    .bind(request.idempotency_key.as_deref())
    .bind(request_fingerprint)
    .execute(conn.as_mut())
    .await
    .map_err(execution_template_admission_sql_error)?;
    let record =
        load_execution_template_admission(&mut conn, request.tenant_id, operation_uid).await?;
    conn.commit().await?;
    Ok(record)
}

/// CAS-records the exact first persisted objective sequence and returns the resulting row.
pub(crate) async fn record_execution_template_admission_origin(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    operation_uid: uuid::Uuid,
    request_fingerprint: &str,
    originating_user_sequence_num: u64,
) -> moa_core::error::Result<ExecutionTemplateAdmissionRecord> {
    let sequence = i64::try_from(originating_user_sequence_num).map_err(|_| {
        moa_core::error::MoaError::ValidationError(
            "execution-template admission objective sequence exceeds BIGINT".to_string(),
        )
    })?;
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    sqlx::query(
        r#"
        UPDATE moa.execution_template_admission
        SET originating_user_sequence_num = $4,
            updated_at = NOW()
        WHERE operation_uid = $1
          AND tenant_id = $2
          AND request_fingerprint = $3
          AND originating_user_sequence_num IS NULL
        "#,
    )
    .bind(operation_uid)
    .bind(tenant_id.0)
    .bind(request_fingerprint)
    .bind(sequence)
    .execute(conn.as_mut())
    .await
    .map_err(execution_template_admission_sql_error)?;
    let record = load_execution_template_admission(&mut conn, tenant_id, operation_uid).await?;
    if record.request_fingerprint != request_fingerprint
        || record.originating_user_sequence_num != Some(originating_user_sequence_num)
    {
        return Err(moa_core::error::MoaError::ValidationError(
            "execution-template admission objective CAS conflicts with first persisted evidence"
                .to_string(),
        ));
    }
    conn.commit().await?;
    Ok(record)
}

/// CAS-records the exact Task 7 run UID and returns the completed operation row.
pub(crate) async fn record_execution_template_admission_run(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    operation_uid: uuid::Uuid,
    request_fingerprint: &str,
    execution_run_uid: uuid::Uuid,
) -> moa_core::error::Result<ExecutionTemplateAdmissionRecord> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    sqlx::query(
        r#"
        UPDATE moa.execution_template_admission
        SET execution_run_uid = $4,
            updated_at = NOW()
        WHERE operation_uid = $1
          AND tenant_id = $2
          AND request_fingerprint = $3
          AND originating_user_sequence_num IS NOT NULL
          AND execution_run_uid IS NULL
        "#,
    )
    .bind(operation_uid)
    .bind(tenant_id.0)
    .bind(request_fingerprint)
    .bind(execution_run_uid)
    .execute(conn.as_mut())
    .await
    .map_err(execution_template_admission_sql_error)?;
    let record = load_execution_template_admission(&mut conn, tenant_id, operation_uid).await?;
    if record.request_fingerprint != request_fingerprint
        || record.execution_run_uid != Some(execution_run_uid)
    {
        return Err(moa_core::error::MoaError::ValidationError(
            "execution-template admission run CAS conflicts with first persisted evidence"
                .to_string(),
        ));
    }
    conn.commit().await?;
    Ok(record)
}

async fn load_execution_template_admission(
    conn: &mut ScopedConn<'_>,
    tenant_id: moa_core::types::identifiers::TenantId,
    operation_uid: uuid::Uuid,
) -> moa_core::error::Result<ExecutionTemplateAdmissionRecord> {
    let row = sqlx::query(
        r#"
        SELECT
            operation_uid,
            request_fingerprint,
            originating_user_sequence_num,
            execution_run_uid
        FROM moa.execution_template_admission
        WHERE operation_uid = $1 AND tenant_id = $2
        "#,
    )
    .bind(operation_uid)
    .bind(tenant_id.0)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(execution_template_admission_sql_error)?
    .ok_or_else(|| {
        moa_core::error::MoaError::StorageError(
            "execution-template admission reservation was not visible after insert".to_string(),
        )
    })?;
    let sequence: Option<i64> = row
        .try_get("originating_user_sequence_num")
        .map_err(execution_template_admission_sql_error)?;
    let originating_user_sequence_num = sequence.map(u64::try_from).transpose().map_err(|_| {
        moa_core::error::MoaError::StorageError(
            "execution-template admission stored a negative objective sequence".to_string(),
        )
    })?;
    Ok(ExecutionTemplateAdmissionRecord {
        operation_uid: row
            .try_get("operation_uid")
            .map_err(execution_template_admission_sql_error)?,
        request_fingerprint: row
            .try_get("request_fingerprint")
            .map_err(execution_template_admission_sql_error)?,
        originating_user_sequence_num,
        execution_run_uid: row
            .try_get("execution_run_uid")
            .map_err(execution_template_admission_sql_error)?,
    })
}

fn execution_template_admission_sql_error(error: sqlx::Error) -> moa_core::error::MoaError {
    moa_core::error::MoaError::StorageError(error.to_string())
}

/// Restate service surface for durable execution-run operations.
#[restate_sdk::service]
#[name = "Execution"]
pub trait Execution {
    /// Derives or replays one immutable origin-bound planning authority snapshot.
    async fn planning_context(
        request: Json<ExecutionPlanningContextRequest>,
    ) -> Result<Json<ExecutionPlanningContextResponse>, HandlerError>;

    /// Creates or idempotently replays one durable execution run.
    async fn start(
        request: Json<ExecutionStartRequest>,
    ) -> Result<Json<ExecutionStartResponse>, HandlerError>;

    /// Confirms one server-gated plan hash and approved budget.
    async fn confirm(
        request: Json<ExecutionConfirmRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Reads one parent-authorized execution run status.
    async fn status(
        request: Json<ExecutionRunRequest>,
    ) -> Result<Json<ExecutionStatusResponse>, HandlerError>;

    /// Loads immutable goal and compact completion evidence for session-owned synthesis.
    async fn synthesis_evidence(
        request: Json<ExecutionSynthesisEvidenceRequest>,
    ) -> Result<Json<ExecutionSynthesisEvidence>, HandlerError>;

    /// Lists a bounded tenant/contact page of execution runs.
    async fn list_runs(
        request: Json<ExecutionRunListRequest>,
    ) -> Result<Json<ExecutionRunListResponse>, HandlerError>;

    /// Lists a bounded page of persisted task results for one run.
    async fn list_tasks(
        request: Json<ExecutionTaskListRequest>,
    ) -> Result<Json<ExecutionTaskListResponse>, HandlerError>;

    /// Cancels one run and all nonterminal tasks.
    async fn cancel(
        request: Json<ExecutionCancelRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Delivers audience-authorized input to one waiting task generation.
    async fn deliver_input(
        request: Json<ExecutionInputRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Resolves one explicit tenant-review task generation.
    async fn decide_review(
        request: Json<ExecutionReviewDecisionRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Delivers one exact named signal to a running task generation.
    async fn deliver_signal(
        request: Json<ExecutionSignalRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Validates and applies an externally supplied amendment.
    async fn apply_amendment(
        request: Json<ExecutionAmendmentRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Applies a workflow-generated amendment using only its persisted run scope.
    async fn apply_planned_amendment(
        request: Json<ExecutionAmendmentRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError>;

    /// Lists the tenant's currently invocable compiler capabilities.
    async fn list_capabilities(
        request: Json<CapabilitiesListRequest>,
    ) -> Result<Json<CapabilitiesListResponse>, HandlerError>;
}

/// Execution service backed by the live tool router and tenant artifact store.
#[derive(Clone)]
pub struct ExecutionImpl {
    pool: sqlx::PgPool,
    router: Arc<ToolRouter>,
    config: ExecutionConfig,
    session_store: Arc<PostgresSessionStore>,
}

impl ExecutionImpl {
    /// Creates the execution service with its live invocation registry.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        router: Arc<ToolRouter>,
        config: ExecutionConfig,
        session_store: Arc<PostgresSessionStore>,
    ) -> Self {
        Self {
            pool,
            router,
            config,
            session_store,
        }
    }
}

impl Execution for ExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn planning_context(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionPlanningContextRequest>,
    ) -> Result<Json<ExecutionPlanningContextResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "planning_context");
        let request = request.into_inner();
        let identity = authorize_session_participant(&ctx, request.session_id).await?;
        let owner_user_id = moa_core::types::identifiers::UserId::new(
            identity
                .acting_on_behalf_of
                .unwrap_or(identity.id)
                .to_string(),
        );
        let store = self.session_store.clone();
        let session_id = request.session_id;
        let sequence = request.originating_user_sequence_num;
        let (parent, event_record) = ctx
            .run(|| async move {
                let parent = store
                    .get_session(session_id)
                    .await
                    .map_err(HandlerError::from)?;
                let record = store
                    .get_events(
                        session_id,
                        EventRange {
                            from_seq: Some(sequence),
                            to_seq: Some(sequence),
                            event_types: None,
                            limit: Some(1),
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?
                    .pop();
                Ok::<_, HandlerError>(Json::from((parent, record)))
            })
            .name("execution_planning_context_origin")
            .await?
            .into_inner();
        let parent_contact_id = parent.contact.as_ref().map(|contact| contact.contact_id);
        if parent.tenant_id != request.tenant_id || parent_contact_id != request.contact_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution planning context scope does not match the authorized parent session",
            )
            .into());
        }
        let Some(event_record) = event_record else {
            return Err(TerminalError::new_with_code(
                409,
                "execution planning context origin event does not exist",
            )
            .into());
        };
        if event_record.sequence_num != request.originating_user_sequence_num
            || !matches!(&event_record.event, Event::UserMessage { .. })
        {
            return Err(TerminalError::new_with_code(
                409,
                "execution planning requires an exact persisted user-message origin",
            )
            .into());
        }
        let pool = self.pool.clone();
        let registrations = self.router.capability_registrations();
        let config = self.config.clone();
        Ok(ctx
            .run(|| async move {
                planning_context_inner(
                    pool,
                    registrations,
                    config,
                    parent,
                    owner_user_id,
                    event_record.event,
                    request,
                )
                .await
                .map(Json::from)
            })
            .name("execution_planning_context")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn start(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionStartRequest>,
    ) -> Result<Json<ExecutionStartResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "start");
        let request = request.into_inner();
        let identity = authorize_session_participant(&ctx, request.session_id).await?;
        let store = self.session_store.clone();
        let session_id = request.session_id;
        let sequence = request.originating_user_sequence_num;
        let (parent, origin) = ctx
            .run(|| async move {
                let parent = store
                    .get_session(session_id)
                    .await
                    .map_err(HandlerError::from)?;
                let origin = store
                    .get_events(
                        session_id,
                        EventRange {
                            from_seq: Some(sequence),
                            to_seq: Some(sequence),
                            event_types: None,
                            limit: Some(1),
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?
                    .pop();
                Ok::<_, HandlerError>(Json::from((parent, origin)))
            })
            .name("execution_start_parent_session")
            .await?
            .into_inner();
        let parent_contact_id = parent.contact.as_ref().map(|contact| contact.contact_id);
        if parent.tenant_id != request.tenant_id || parent_contact_id != request.contact_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution scope does not match the authorized parent session",
            )
            .into());
        }
        let objective = match origin.map(|record| record.event) {
            Some(Event::UserMessage { text, .. }) => text,
            _ => {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution start requires an exact persisted user-message origin",
                )
                .into());
            }
        };
        let pool = self.pool.clone();
        let config = self.config.clone();
        let response = ctx
            .run(|| async move {
                start_inner(pool, config, request, objective)
                    .await
                    .map(Json::from)
            })
            .name("execution_start")
            .await?
            .into_inner();
        tracing::Span::current()
            .set_attribute("moa.execution.run_uid", response.run.run_uid.to_string());
        with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .execution_run_started(Json::from(execution_run_started_delivery(&response))),
            &identity,
        )
        .send();
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn confirm(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionConfirmRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "confirm");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.run.session_id).await?;
        let run_request = request.run.clone();
        let pool = self.pool.clone();
        let accepted = ctx
            .run(|| async move { confirm_inner(pool, request).await.map(Json::from) })
            .name("execution_confirm")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            send_run_wake(
                &ctx,
                run_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::Confirmed,
            );
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRunRequest>,
    ) -> Result<Json<ExecutionStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "status");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.session_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { status_inner(pool, request).await.map(Json::from) })
            .name("execution_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn synthesis_evidence(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionSynthesisEvidenceRequest>,
    ) -> Result<Json<ExecutionSynthesisEvidence>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "synthesis_evidence");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.run.session_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                synthesis_evidence_inner(pool, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_synthesis_evidence")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_runs(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRunListRequest>,
    ) -> Result<Json<ExecutionRunListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "list_runs");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_runs_inner(pool, request).await.map(Json::from) })
            .name("execution_list_runs")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_tasks(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionTaskListRequest>,
    ) -> Result<Json<ExecutionTaskListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "list_tasks");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.run.session_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_tasks_inner(pool, request).await.map(Json::from) })
            .name("execution_list_tasks")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionCancelRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "cancel");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.run.session_id).await?;
        let run_request = request.run.clone();
        let pool = self.pool.clone();
        let accepted = ctx
            .run(|| async move { cancel_inner(pool, request).await.map(Json::from) })
            .name("execution_cancel")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            send_run_wake(
                &ctx,
                run_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::Cancelled,
            );
        }
        for task_id in accepted.task_ids_to_release() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                    .cancel(Json::from("execution run cancelled".to_string())),
            )
            .send();
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn deliver_input(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionInputRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "deliver_input");
        let request = request.into_inner();
        match request.audience {
            moa_artifacts::execution_plan::InputAudience::User => {
                let session_id = request.session_id.ok_or_else(|| {
                    TerminalError::new_with_code(400, "user input requires session_id")
                })?;
                authorize_session_participant(&ctx, session_id).await?;
            }
            moa_artifacts::execution_plan::InputAudience::TenantAdmin
            | moa_artifacts::execution_plan::InputAudience::ExternalSystem => {
                authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
            }
        }
        let task_request = request.clone();
        let pool = self.pool.clone();
        let accepted = ctx
            .run(|| async move { deliver_input_inner(pool, request).await.map(Json::from) })
            .name("execution_deliver_input")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            if accepted
                .task_ids_to_release()
                .contains(&task_request.task_id)
            {
                crate::restate_identity::replay_safe_request(
                    ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                        .cancel(Json::from(
                            "execution input redispatch reached a terminal admission outcome"
                                .to_string(),
                        )),
                )
                .send();
            } else {
                crate::restate_identity::replay_safe_request(
                    ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                        .input_delivered(Json::from(task_request.clone())),
                )
                .send();
            }
            send_run_wake(
                &ctx,
                task_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::InputDelivered,
            );
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide_review(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionReviewDecisionRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "decide_review");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let task_request = request.clone();
        let pool = self.pool.clone();
        let accepted = ctx
            .run(|| async move { decide_review_inner(pool, request).await.map(Json::from) })
            .name("execution_decide_review")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                    .review_decided(Json::from(task_request.clone())),
            )
            .send();
            send_run_wake(
                &ctx,
                task_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::ReviewDecided,
            );
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn deliver_signal(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionSignalRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "deliver_signal");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let task_request = request.clone();
        let pool = self.pool.clone();
        let accepted = ctx
            .run(|| async move { deliver_signal_inner(pool, request).await.map(Json::from) })
            .name("execution_deliver_signal")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                    .signal_delivered(Json::from(task_request.clone())),
            )
            .send();
            send_run_wake(
                &ctx,
                task_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::SignalDelivered,
            );
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn apply_amendment(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionAmendmentRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "apply_amendment");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.run.session_id).await?;
        let run_uid = request.run.run_uid;
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                apply_amendment_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_apply_amendment")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            send_run_wake(
                &ctx,
                run_uid,
                wake_epoch,
                ExecutionRunWakeReason::AmendmentAccepted,
            );
        }
        for task_id in accepted.task_ids_to_release() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                    .cancel(Json::from(
                        "execution task superseded or stopped by amendment".to_string(),
                    )),
            )
            .send();
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only by the keyed ExecutionRun workflow; the request carries no authority and apply_amendment_inner reloads and revision-fences all persisted scope.
    async fn apply_planned_amendment(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionAmendmentRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "apply_planned_amendment");
        let request = request.into_inner();
        let run_uid = request.run.run_uid;
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                apply_amendment_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_apply_planned_amendment")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            send_run_wake(
                &ctx,
                run_uid,
                wake_epoch,
                ExecutionRunWakeReason::AmendmentAccepted,
            );
        }
        for task_id in accepted.task_ids_to_release() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                    .cancel(Json::from(
                        "execution task superseded or stopped by amendment".to_string(),
                    )),
            )
            .send();
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_capabilities(
        &self,
        ctx: Context<'_>,
        request: Json<CapabilitiesListRequest>,
    ) -> Result<Json<CapabilitiesListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "list_capabilities");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        let pool = self.pool.clone();
        let registrations = self.router.capability_registrations();
        Ok(ctx
            .run(|| async move {
                list_capabilities_inner(pool, registrations, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_list_capabilities")
            .await?)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ExecutionMutationHandoff {
    wake_epoch: u64,
    task_ids_to_release: Vec<moa_execution::state::ExecutionTaskId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum ExecutionMutationAccepted {
    Accepted {
        response: ExecutionMutationResponse,
        handoff: ExecutionMutationHandoff,
    },
    Rejected {
        response: ExecutionMutationResponse,
    },
}

impl ExecutionMutationAccepted {
    fn wake_epoch(&self) -> Option<u64> {
        match self {
            Self::Accepted { handoff, .. } => Some(handoff.wake_epoch),
            Self::Rejected { .. } => None,
        }
    }

    fn task_ids_to_release(&self) -> &[moa_execution::state::ExecutionTaskId] {
        match self {
            Self::Accepted { handoff, .. } => &handoff.task_ids_to_release,
            Self::Rejected { .. } => &[],
        }
    }

    fn with_task_ids_to_release(
        mut self,
        task_ids_to_release: Vec<moa_execution::state::ExecutionTaskId>,
    ) -> Self {
        if let Self::Accepted { handoff, .. } = &mut self {
            handoff.task_ids_to_release = task_ids_to_release;
        }
        self
    }

    fn into_response(self) -> ExecutionMutationResponse {
        match self {
            Self::Accepted { response, .. } | Self::Rejected { response } => response,
        }
    }
}

async fn planning_context_inner(
    pool: sqlx::PgPool,
    registrations: Vec<(ToolDefinition, ToolExecution)>,
    config: ExecutionConfig,
    parent: moa_core::types::session::SessionMeta,
    owner_user_id: moa_core::types::identifiers::UserId,
    originating_event: Event,
    request: ExecutionPlanningContextRequest,
) -> Result<ExecutionPlanningContextResponse, HandlerError> {
    let registrations = registrations
        .into_iter()
        .filter_map(|registration| {
            let allowed = parent
                .agent_context
                .as_ref()
                .map(|context| context.allows_tool(&registration.0.name))
                .transpose();
            match allowed {
                Ok(Some(false)) => None,
                Ok(Some(true) | None) => Some(Ok(registration)),
                Err(error) => Some(Err(invalid_execution_request(format!(
                    "invalid session tool policy: {error}"
                )))),
            }
        })
        .collect::<Result<Vec<_>, HandlerError>>()?;
    let scope = request.contact_id.map_or(
        ActionRuleScope::Tenant {
            tenant_id: request.tenant_id,
        },
        |contact_id| ActionRuleScope::Contact {
            tenant_id: request.tenant_id,
            contact_id,
        },
    );
    let registry = ArtifactRegistry::new(pool.clone());
    let revisions = load_published_revisions(&registry, &scope)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let skill_policy = parent
        .agent_context
        .as_ref()
        .map(|context| context.parsed_policy_snapshot())
        .transpose()
        .map_err(|error| {
            invalid_execution_request(format!("invalid session skill policy: {error}"))
        })?
        .map(|snapshot| snapshot.skill_policy)
        .unwrap_or_default();
    let locked_skill_revisions =
        load_locked_skill_revisions(&registry, &scope, parent.agent_context.as_ref()).await?;
    let skill_context = build_planning_skill_context(
        revisions,
        locked_skill_revisions,
        &skill_policy,
        request.requested_template.as_ref(),
    )
    .map_err(invalid_execution_request)?;
    let connection_refs = load_connection_refs(pool.clone(), request.tenant_id)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let capability_response =
        build_capability_response(&registrations, &skill_context.revisions, &connection_refs)
            .map_err(execution_error)?;

    let pinned_instruction_skills = skill_context.pinned_instruction_skills;
    let execution_templates = skill_context.execution_templates;

    let skill_refs = pinned_instruction_skills
        .iter()
        .map(|skill| skill.skill_ref.clone())
        .collect::<Vec<_>>();
    let authorization = moa_execution::ExecutionAuthorizationEnvelope {
        capability_refs: capability_response
            .catalog
            .capabilities
            .iter()
            .map(|capability| capability.reference.clone())
            .collect(),
        skill_refs,
    };
    let event_hash = originating_user_event_hash(
        request.session_id,
        request.originating_user_sequence_num,
        &originating_event,
    )
    .map_err(execution_error)?;
    let snapshot = ExecutionPlanningContextSnapshot {
        schema_version: 1,
        tenant_id: request.tenant_id,
        contact_id: request.contact_id,
        session_id: request.session_id,
        originating_user_sequence_num: request.originating_user_sequence_num,
        originating_user_event_hash: event_hash.to_string(),
        owner_user_id,
        catalog: capability_response.catalog,
        authorization,
        pinned_instruction_skills,
        execution_templates,
        budget: ExecutionBudgetLimit {
            max_cost_microusd: Some(config.max_cost_microusd),
            max_tokens: Some(config.max_tokens),
            max_tasks: Some(config.max_tasks),
            max_tool_calls: Some(config.max_tool_calls),
            max_retrieved_bytes: Some(config.max_retrieved_bytes),
            deadline_at: None,
        },
    };
    let hash = planning_context_hash(&snapshot).map_err(execution_error)?;
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    match repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot,
                planning_context_hash: hash,
            },
        )
        .await
        .map_err(execution_error)?
    {
        PlanningContextWriteOutcome::Created(record) => Ok(ExecutionPlanningContextResponse {
            planning_context_uid: record.planning_context_uid,
            planning_context_hash: record.planning_context_hash.to_string(),
            snapshot: record.snapshot,
            created: true,
        }),
        PlanningContextWriteOutcome::Replayed(record) => Ok(ExecutionPlanningContextResponse {
            planning_context_uid: record.planning_context_uid,
            planning_context_hash: record.planning_context_hash.to_string(),
            snapshot: record.snapshot,
            created: false,
        }),
        PlanningContextWriteOutcome::Conflict => Err(TerminalError::new_with_code(
            409,
            "originating user event already has a different planning context",
        )
        .into()),
    }
}

#[derive(Debug)]
struct PlanningSkillContext {
    revisions: Vec<StoredArtifactRevision>,
    pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    execution_templates: Vec<PinnedExecutionTemplate>,
}

fn build_planning_skill_context(
    revisions: Vec<StoredArtifactRevision>,
    locked_skill_revisions: Vec<StoredArtifactRevision>,
    policy: &AgentSkillPolicy,
    requested_template: Option<&moa_core::types::execution_planning::PinnedExecutionTemplateRef>,
) -> Result<PlanningSkillContext, String> {
    let policy_refs = policy
        .refs
        .iter()
        .map(|reference| canonical_skill_policy_ref(reference))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut non_skill_revisions = Vec::new();
    let mut published_skills = BTreeMap::new();
    for revision in revisions {
        if !matches!(revision.document.definition, ArtifactDefinition::Skill(_)) {
            non_skill_revisions.push(revision);
            continue;
        }
        let reference = skill_revision_ref(&revision)?;
        if let Some(previous) = published_skills.insert(reference.clone(), revision) {
            let duplicate_uid = published_skills
                .get(&reference)
                .map(|current| current.revision_uid == previous.revision_uid)
                .unwrap_or(false);
            return Err(if duplicate_uid {
                format!(
                    "duplicate exact skill revision: {reference}@{}",
                    previous.revision_uid
                )
            } else {
                format!("multiple published revisions for planning skill: {reference}")
            });
        }
    }

    let mut locked_skills = BTreeMap::new();
    for revision in locked_skill_revisions {
        let reference = skill_revision_ref(&revision)?;
        if let Some(previous) = locked_skills.insert(reference.clone(), revision) {
            let duplicate_uid = locked_skills
                .get(&reference)
                .map(|current| current.revision_uid == previous.revision_uid)
                .unwrap_or(false);
            return Err(if duplicate_uid {
                format!(
                    "duplicate exact locked skill revision: {reference}@{}",
                    previous.revision_uid
                )
            } else {
                format!("multiple locked revisions for planning skill: {reference}")
            });
        }
    }
    let mut ordered = published_skills.into_iter().collect::<Vec<_>>();
    match policy.mode {
        moa_core::types::agent::AgentSkillPolicyMode::Auto => {}
        moa_core::types::agent::AgentSkillPolicyMode::Allowlist => {
            ordered.retain(|(reference, _)| policy_refs.contains(reference));
        }
        moa_core::types::agent::AgentSkillPolicyMode::Denylist => {
            ordered.retain(|(reference, _)| !policy_refs.contains(reference));
        }
        moa_core::types::agent::AgentSkillPolicyMode::Pinned => {
            ordered.sort_by_key(|(reference, _)| {
                (!policy_refs.contains(reference), reference.clone())
            });
        }
    }
    if let Some(max_visible) = policy.max_visible {
        let limit = usize::try_from(max_visible)
            .map_err(|_| "agent skill max_visible does not fit usize".to_string())?;
        ordered.truncate(limit);
    }
    for (reference, revision) in &mut ordered {
        if let Some(locked) = locked_skills.remove(reference) {
            *revision = locked;
        }
    }
    ordered.sort_by(|(left_ref, left), (right_ref, right)| {
        left_ref
            .cmp(right_ref)
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });

    let selected_skills = ordered
        .into_iter()
        .map(|(_, revision)| revision)
        .collect::<Vec<_>>();
    let mut pinned_instruction_skills = Vec::new();
    let mut execution_templates = Vec::new();
    for revision in &selected_skills {
        let ArtifactDefinition::Skill(skill) = &revision.document.definition else {
            continue;
        };
        let skill_ref = ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone());
        pinned_instruction_skills.push(PinnedInstructionSkill {
            skill_ref: skill_ref.clone(),
            revision_uid: revision.revision_uid,
        });
        if let Some(execution_plan) = &skill.execution_plan {
            execution_templates.push(PinnedExecutionTemplate {
                skill_ref,
                revision_uid: revision.revision_uid,
                skill_input_schema: skill.inputs.clone(),
                execution_plan: execution_plan.clone(),
            });
        }
    }
    pinned_instruction_skills.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });
    execution_templates.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });

    if let Some(requested) = requested_template {
        let parsed = requested
            .skill_ref
            .parse::<ArtifactRef>()
            .map_err(|error| format!("invalid execution template ref: {error}"))?;
        if parsed.to_string() != requested.skill_ref
            || execution_templates
                .iter()
                .filter(|template| {
                    template.skill_ref == parsed && template.revision_uid == requested.revision_uid
                })
                .count()
                != 1
        {
            return Err(
                "requested execution template is not an exact permitted pinned published revision"
                    .to_string(),
            );
        }
    }

    non_skill_revisions.extend(selected_skills);

    Ok(PlanningSkillContext {
        revisions: non_skill_revisions,
        pinned_instruction_skills,
        execution_templates,
    })
}

fn canonical_skill_policy_ref(reference: &str) -> Result<String, String> {
    let parsed = reference
        .parse::<ArtifactRef>()
        .map_err(|error| format!("invalid agent skill policy ref `{reference}`: {error}"))?;
    if !matches!(
        parsed,
        ArtifactRef::Artifact {
            kind: ArtifactKind::Skill,
            ..
        }
    ) || parsed.to_string() != reference
    {
        return Err(format!(
            "agent skill policy ref must be canonical skill:// reference: {reference}"
        ));
    }
    Ok(reference.to_string())
}

fn skill_revision_ref(revision: &StoredArtifactRevision) -> Result<String, String> {
    if revision.status != ArtifactStatus::Published
        || !matches!(revision.document.definition, ArtifactDefinition::Skill(_))
    {
        return Err(format!(
            "planning skill revision is not published skill content: {}",
            revision.revision_uid
        ));
    }
    ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone())
        .canonical_string()
        .map_err(|error| format!("invalid planning skill revision ref: {error}"))
}

async fn start_inner(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionStartRequest,
    originating_objective: String,
) -> Result<ExecutionStartResponse, HandlerError> {
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let repository = ExecutionRepository::new(pool);
    let planning_context = repository
        .load_planning_context(scope, request.planning_context_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| {
            TerminalError::new_with_code(409, "execution planning context does not exist")
        })?;
    let expected_context_hash = request
        .planning_context_hash
        .parse::<ExecutionHash>()
        .map_err(execution_error)?;
    let snapshot = &planning_context.snapshot;
    if planning_context.planning_context_hash != expected_context_hash
        || snapshot.tenant_id != request.tenant_id
        || snapshot.contact_id != request.contact_id
        || snapshot.session_id != request.session_id
        || snapshot.originating_user_sequence_num != request.originating_user_sequence_num
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution planning context hash or origin scope mismatch",
        )
        .into());
    }
    if request.compiled.goal.objective.as_bytes() != originating_objective.as_bytes() {
        return Err(invalid_execution_request(
            "compiled execution objective must equal the persisted user message",
        ));
    }
    let validation = compile(CompileExecutionRequest {
        goal: request.compiled.goal.clone(),
        plan: request.compiled.plan.definition.clone(),
        run_input: request.run_input.clone(),
        catalog: snapshot.catalog.clone(),
        authorization: snapshot.authorization.clone(),
        approved_budget: snapshot.budget.clone(),
        config: config.clone(),
        now: Utc::now(),
    });
    if validation.compiled.as_ref() != Some(&request.compiled) {
        return Err(invalid_execution_request(
            "compiled execution does not match deterministic server validation",
        ));
    }
    if request.compiled.plan.plan_hash
        != plan_hash(&request.compiled.plan.definition).map_err(execution_error)?
        || request.compiled.plan.catalog_hash != snapshot.catalog.catalog_hash
    {
        return Err(invalid_execution_request(
            "compiled plan hashes do not match the supplied immutable snapshots",
        ));
    }
    estimate_fits_limit(request.compiled.plan.estimate, &snapshot.budget)
        .map_err(execution_error)?;
    validate_start_source_provenance(
        &request.source_provenance,
        &request.compiled.plan.plan_hash.to_string(),
        &snapshot.execution_templates,
    )
    .map_err(|error| invalid_execution_request(error.to_string()))?;
    let existing = if let Some(key) = request.idempotency_key.as_deref() {
        repository
            .load_run_by_idempotency_key(scope, request.tenant_id, request.contact_id, key)
            .await
            .map_err(execution_error)?
    } else {
        None
    };
    if let Some(run) = existing {
        verify_run_scope(
            &run,
            request.tenant_id,
            request.contact_id,
            request.session_id,
        )?;
        verify_start_replay(&run, &request, snapshot)?;
        let confirmation_required = run.status == ExecutionRunStatus::AwaitingConfirmation;
        return Ok(ExecutionStartResponse {
            active_plan_hash: run.active_plan_hash,
            estimate: run.active_plan.estimate,
            run: run_summary(&run),
            created: false,
            confirmation_required,
        });
    }
    let confirmation_required =
        request.compiled.plan.estimate.cost_microusd > config.unattended_max_cost_microusd;
    let status = if confirmation_required {
        ExecutionRunStatus::AwaitingConfirmation
    } else {
        ExecutionRunStatus::Queued
    };
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: request.tenant_id,
                contact_id: request.contact_id,
                session_id: request.session_id,
                originating_user_sequence_num: request.originating_user_sequence_num,
                planning_context_uid: request.planning_context_uid,
                planning_context_hash: expected_context_hash,
                owner_user_id: snapshot.owner_user_id.clone(),
                goal: request.compiled.goal,
                plan: request.compiled.plan,
                catalog: snapshot.catalog.clone(),
                authorization: snapshot.authorization.clone(),
                pinned_instruction_skills: snapshot.pinned_instruction_skills.clone(),
                source_provenance: request.source_provenance,
                input: request.run_input,
                status,
                approved_budget: snapshot.budget.clone(),
                idempotency_key: request.idempotency_key,
            },
        )
        .await
        .map_err(execution_error)?;
    record_applied_run_transition(None, &run);
    Ok(ExecutionStartResponse {
        active_plan_hash: run.active_plan_hash,
        estimate: run.active_plan.estimate,
        run: run_summary(&run),
        created: true,
        confirmation_required,
    })
}

fn validate_start_source_provenance(
    provenance: &ExecutionSourceProvenance,
    committed_plan_hash: &str,
    execution_templates: &[PinnedExecutionTemplate],
) -> Result<(), ExecutionPlanningContractError> {
    provenance.validate(committed_plan_hash)?;
    let (skill_template_ref, skill_template_revision_uid) = match provenance {
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        }
        | ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => (skill_template_ref, skill_template_revision_uid),
        ExecutionSourceProvenance::GeneratedPlan { .. } => return Ok(()),
    };
    let parsed = skill_template_ref.parse::<ArtifactRef>().map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message: error.to_string(),
        }
    })?;
    let canonical = parsed.canonical_string().map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message: error.to_string(),
        }
    })?;
    if canonical != *skill_template_ref
        || !execution_templates
            .iter()
            .any(|template| template.skill_ref == parsed)
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message:
                "must equal one canonical template reference in the persisted planning context"
                    .to_string(),
        });
    }
    if execution_templates
        .iter()
        .filter(|template| {
            template.skill_ref == parsed && template.revision_uid == *skill_template_revision_uid
        })
        .count()
        != 1
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_revision_uid".to_string(),
            message: "must equal one exact template revision in the persisted planning context"
                .to_string(),
        });
    }
    Ok(())
}

async fn confirm_inner(
    pool: sqlx::PgPool,
    request: ExecutionConfirmRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&run, &request.run)?;
    let outcome = repository
        .confirm_run(
            scope,
            run.run_uid,
            &request.expected_plan_hash,
            request.approved_budget,
        )
        .await
        .map_err(execution_error)?;
    let prior_status = run.status;
    Ok(match outcome {
        ConfirmationOutcome::Confirmed(run) => {
            record_applied_run_transition(Some(prior_status), &run);
            applied_mutation(&run)
        }
        ConfirmationOutcome::AlreadyConfirmed(run) => replayed_mutation(&run),
        ConfirmationOutcome::NotFound => not_found_mutation(),
        ConfirmationOutcome::Conflict(reason) => conflict_mutation(match reason {
            ConfirmationConflict::PlanHashMismatch => ExecutionConflictReason::PlanHashMismatch,
            ConfirmationConflict::BudgetMismatch => ExecutionConflictReason::BudgetMismatch,
            ConfirmationConflict::InvalidStatus => ExecutionConflictReason::InvalidStatus,
        }),
    })
}

async fn status_inner(
    pool: sqlx::PgPool,
    request: ExecutionRunRequest,
) -> Result<ExecutionStatusResponse, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let run = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    verify_run_request(&run, &request)?;
    Ok(ExecutionStatusResponse {
        run: run_summary(&run),
        waiting: run.waiting_reasons.clone(),
        output: run.output.clone(),
        gaps: run.terminal_gaps.clone(),
    })
}

async fn synthesis_evidence_inner(
    pool: sqlx::PgPool,
    request: ExecutionSynthesisEvidenceRequest,
) -> Result<ExecutionSynthesisEvidence, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let run = repository
        .load_run(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    verify_run_request(&run, &request.run)?;
    if run.originating_user_sequence_num != request.originating_user_sequence_num {
        return Err(TerminalError::new_with_code(
            409,
            "execution synthesis origin does not match the durable run",
        )
        .into());
    }
    if !run.status.is_terminal() {
        return Err(TerminalError::new_with_code(
            409,
            "execution synthesis evidence is available only for terminal runs",
        )
        .into());
    }
    Ok(ExecutionSynthesisEvidence {
        goal: run.goal,
        completion_check_results: run.completion_check_results,
    })
}

async fn list_runs_inner(
    pool: sqlx::PgPool,
    request: ExecutionRunListRequest,
) -> Result<ExecutionRunListResponse, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_cursor::<ExecutionRunCursor>)
        .transpose()
        .map_err(execution_error)?;
    if let Some(cursor) = cursor {
        let boundary = repository
            .load_run(scope, cursor.run_uid)
            .await
            .map_err(execution_error)?;
        if boundary.as_ref().map(|run| run.created_at) != Some(cursor.created_at) {
            return Err(invalid_execution_request(
                "run cursor does not belong to the requested scope",
            ));
        }
    }
    let page = repository
        .list_runs(
            scope,
            ExecutionRunPageRequest {
                limit: request.limit.unwrap_or_default(),
                cursor: cursor.map(|cursor| moa_execution::repository::ExecutionRunCursor {
                    created_at: cursor.created_at,
                    run_uid: cursor.run_uid,
                }),
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(ExecutionRunListResponse {
        runs: page.runs.iter().map(run_summary).collect(),
        next_cursor: page
            .next_cursor
            .map(|cursor| ExecutionRunCursor {
                created_at: cursor.created_at,
                run_uid: cursor.run_uid,
            })
            .map(|cursor| encode_cursor(&cursor))
            .transpose()
            .map_err(execution_error)?,
    })
}

async fn list_tasks_inner(
    pool: sqlx::PgPool,
    request: ExecutionTaskListRequest,
) -> Result<ExecutionTaskListResponse, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let run = repository
        .load_run(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    verify_run_request(&run, &request.run)?;
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_cursor::<ExecutionTaskCursor>)
        .transpose()
        .map_err(execution_error)?;
    if let Some(cursor) = cursor.as_ref() {
        let boundary = repository
            .load_task(scope, run.run_uid, cursor.task_id)
            .await
            .map_err(execution_error)?;
        if !boundary
            .as_ref()
            .is_some_and(|task| task.node_id == cursor.node_id && task.item_key == cursor.item_key)
        {
            return Err(invalid_execution_request(
                "task cursor does not belong to the requested run",
            ));
        }
    }
    let page = repository
        .list_tasks(
            scope,
            run.run_uid,
            ExecutionTaskPageRequest {
                limit: request.limit.unwrap_or_default(),
                cursor: cursor.map(|cursor| moa_execution::repository::ExecutionTaskCursor {
                    node_id: cursor.node_id,
                    item_key: cursor.item_key,
                    task_id: cursor.task_id,
                }),
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(ExecutionTaskListResponse {
        tasks: page.tasks.iter().map(task_projection).collect(),
        next_cursor: page
            .next_cursor
            .map(|cursor| ExecutionTaskCursor {
                node_id: cursor.node_id,
                item_key: cursor.item_key,
                task_id: cursor.task_id,
            })
            .map(|cursor| encode_cursor(&cursor))
            .transpose()
            .map_err(execution_error)?,
    })
}

async fn cancel_inner(
    pool: sqlx::PgPool,
    request: ExecutionCancelRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&snapshot.run, &request.run)?;
    let terminal_evidence = cancellation_terminal_evidence(
        &snapshot.run.goal,
        &snapshot.run.active_plan,
        &snapshot.projection,
    )
    .map_err(execution_error)?;
    Ok(
        match repository
            .cancel_run(
                scope,
                snapshot.run.run_uid,
                CancellationRequest {
                    reason: request.reason,
                    terminal_evidence,
                },
            )
            .await
            .map_err(execution_error)?
        {
            CancellationOutcome::Cancelled { commit, metrics } => {
                record_applied_execution_mutation(&metrics);
                applied_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
            }
            CancellationOutcome::Replayed(commit) => {
                replayed_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
            }
            CancellationOutcome::NotFound => not_found_mutation(),
            CancellationOutcome::Conflict => {
                conflict_mutation(ExecutionConflictReason::AlreadyTerminal)
            }
        },
    )
}

async fn deliver_input_inner(
    pool: sqlx::PgPool,
    request: ExecutionInputRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    if run.tenant_id != request.tenant_id
        || run.contact_id != request.contact_id
        || request
            .session_id
            .is_some_and(|session_id| run.session_id != session_id)
    {
        return Ok(conflict_mutation(ExecutionConflictReason::ScopeMismatch));
    }
    let Some(task) = repository
        .load_task(scope, run.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    let persisted_audience = persisted_input_audience(
        task.generation,
        task.current_outcome.as_ref(),
        &task.outcome_audit,
        request.expected_generation,
    );
    if persisted_audience.as_ref() != Some(&request.audience) {
        return Ok(conflict_mutation(ExecutionConflictReason::AudienceMismatch));
    }
    let transition = repository
        .resume_task_with_input(
            scope,
            run.run_uid,
            task.task_id,
            request.expected_generation,
            request.input,
        )
        .await
        .map_err(execution_error)?;
    mutation_from_transition(&repository, scope, run.run_uid, transition).await
}

fn persisted_input_audience(
    current_generation: u64,
    current_outcome: Option<&ExecutionTaskOutcome>,
    outcome_audit: &[Value],
    expected_generation: u64,
) -> Option<moa_artifacts::execution_plan::InputAudience> {
    if current_generation == expected_generation
        && let Some(ExecutionTaskResult::NeedsInput { audience, .. }) =
            current_outcome.map(|outcome| &outcome.result)
    {
        return Some(audience.clone());
    }
    outcome_audit.iter().rev().find_map(|entry| {
        if entry.get("received_generation").and_then(Value::as_u64) != Some(expected_generation)
            || entry.get("accepted").and_then(Value::as_bool) != Some(true)
        {
            return None;
        }
        let outcome =
            serde_json::from_value::<ExecutionTaskOutcome>(entry.get("outcome")?.clone()).ok()?;
        match outcome.result {
            ExecutionTaskResult::NeedsInput { audience, .. } => Some(audience),
            ExecutionTaskResult::Completed { .. }
            | ExecutionTaskResult::NeedsReplan { .. }
            | ExecutionTaskResult::Cancelled { .. }
            | ExecutionTaskResult::Failed { .. } => None,
        }
    })
}

async fn decide_review_inner(
    pool: sqlx::PgPool,
    request: ExecutionReviewDecisionRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    if run.tenant_id != request.tenant_id || run.contact_id != request.contact_id {
        return Ok(conflict_mutation(ExecutionConflictReason::ScopeMismatch));
    }
    let Some(task) = repository
        .load_task(scope, run.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    if !matches!(
        task.kind,
        moa_execution::state::LogicalTaskKind::Review { .. }
    ) {
        return Ok(conflict_mutation(ExecutionConflictReason::InvalidStatus));
    }
    let result = match request.decision {
        ExecutionReviewDecision::Approved { payload } => ExecutionTaskResult::Completed {
            output: payload,
            citations: Vec::new(),
        },
        ExecutionReviewDecision::Rejected { reason } => ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::AuthorizationDenied,
            message: reason,
        },
    };
    external_wait_mutation(
        &repository,
        scope,
        &run,
        &task,
        request.expected_generation,
        result,
    )
    .await
}

async fn deliver_signal_inner(
    pool: sqlx::PgPool,
    request: ExecutionSignalRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    if run.tenant_id != request.tenant_id || run.contact_id != request.contact_id {
        return Ok(conflict_mutation(ExecutionConflictReason::ScopeMismatch));
    }
    let Some(task) = repository
        .load_task(scope, run.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    let signal_matches = matches!(
        &task.kind,
        moa_execution::state::LogicalTaskKind::WaitSignal { signal_name }
            if signal_name == &request.signal_name
    );
    if !signal_matches {
        return Ok(conflict_mutation(ExecutionConflictReason::SignalMismatch));
    }
    external_wait_mutation(
        &repository,
        scope,
        &run,
        &task,
        request.expected_generation,
        ExecutionTaskResult::Completed {
            output: request.payload,
            citations: Vec::new(),
        },
    )
    .await
}

async fn external_wait_mutation(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    generation: u64,
    result: ExecutionTaskResult,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    if task.generation == generation
        && task.status == ExecutionTaskStatus::Running
        && let ExecutionTaskResult::Completed { output, .. } = &result
    {
        validate_external_wait_payload(&run.active_plan.definition, &task.node_id, output)?;
    }
    let write = repository
        .complete_external_wait(
            scope,
            run.run_uid,
            task.task_id,
            generation,
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: zero_usage(),
                result,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(mutation_from_task_write(write))
}

fn validate_external_wait_payload(
    plan: &ExecutionPlanDefinition,
    node_id: &str,
    payload: &Value,
) -> Result<(), HandlerError> {
    let schema = plan
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .map(|node| &node.output_schema)
        .ok_or_else(|| invalid_execution_request("waiting task node is absent from active plan"))?;
    validate_instance(schema, payload, "execution.external_wait_output")
        .map_err(|error| invalid_execution_request(format!("invalid external payload: {error}")))
}

async fn apply_amendment_inner(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionAmendmentRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&snapshot.run, &request.run)?;
    let amendment_digest = amendment_hash(&request.amendment).map_err(execution_error)?;
    match repository
        .recover_amendment_handoff(
            scope,
            snapshot.run.run_uid,
            request.expected_plan_revision,
            &amendment_digest,
        )
        .await
        .map_err(execution_error)?
    {
        AmendmentReplayOutcome::Replayed(commit) => {
            return Ok(
                replayed_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
            );
        }
        AmendmentReplayOutcome::NotFound => return Ok(not_found_mutation()),
        AmendmentReplayOutcome::Conflict => {
            return Ok(conflict_mutation(
                ExecutionConflictReason::PlanRevisionMismatch,
            ));
        }
        AmendmentReplayOutcome::NotApplied => {}
    }
    if snapshot.run.plan_revision != request.expected_plan_revision {
        return Ok(conflict_mutation(
            ExecutionConflictReason::PlanRevisionMismatch,
        ));
    }
    let remaining_budget = snapshot
        .budget_ledger
        .remaining_limit()
        .map_err(execution_error)?;
    let waiting_tasks = snapshot
        .projection
        .tasks
        .iter()
        .filter(|task| task.status == ExecutionTaskStatus::WaitingReplan)
        .collect::<Vec<_>>();
    let [waiting_task] = waiting_tasks.as_slice() else {
        return Ok(conflict_mutation(ExecutionConflictReason::InvalidStatus));
    };
    let proposed_amendment_fingerprint =
        amendment_operations_fingerprint(&request.amendment).map_err(execution_error)?;
    let loop_evaluation = replan_loop_evaluation_request(
        &snapshot,
        proposed_amendment_fingerprint,
        request.amendment.clone(),
        config.clone(),
        waiting_task,
    )
    .map_err(execution_error)?;
    // Detect loop identity and structural no-progress before compiler rejection can hide the
    // typed stop. Valid candidates are still evaluated through the complete precedence below.
    let prevalidation_loop_decision = evaluate_replan_loop_stop(loop_evaluation.clone());
    let now = chrono::Utc::now();
    let validated = validate_amendment(ValidateAmendmentRequest {
        goal: snapshot.run.goal.clone(),
        active_plan: snapshot.run.active_plan.clone(),
        amendment: request.amendment.clone(),
        projection: snapshot.projection.clone(),
        catalog: snapshot.catalog.clone(),
        authorization: snapshot.authorization.clone(),
        remaining_budget: remaining_budget.clone(),
        config: config.clone(),
        now,
    });
    if let Some(remaining_estimate) = validated.remaining_estimate
        && let Some(reason) =
            evaluate_replan_resource_stop(now, &remaining_budget, remaining_estimate)
    {
        return finalize_service_replan_stop(
            &repository,
            scope,
            &snapshot,
            waiting_task,
            amendment_digest,
            reason,
            Some(&request.amendment.reason),
        )
        .await;
    }
    let Some(active_plan) = validated.plan else {
        if let ReplanDecision::Stop { reason } = prevalidation_loop_decision {
            return finalize_service_replan_stop(
                &repository,
                scope,
                &snapshot,
                waiting_task,
                amendment_digest,
                reason,
                Some(&request.amendment.reason),
            )
            .await;
        }
        return Err(invalid_execution_request(format!(
            "amendment validation failed: {:?}",
            validated.report.issues
        )));
    };
    let proposed_estimate = validated.remaining_estimate.ok_or_else(|| {
        invalid_execution_request("validated amendment is missing its remaining-work estimate")
    })?;
    let requirement_mapping = request
        .amendment
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PlanAmendmentOperation::AddNode { node }
            | PlanAmendmentOperation::ReplacePendingNode { node, .. } => {
                Some((node.id.clone(), node.requirement_ids.clone()))
            }
            PlanAmendmentOperation::RemovePendingNode { .. } => None,
        })
        .collect();
    if let ReplanDecision::Stop { reason } = evaluate_replan_stop(replan_evaluation_request(
        &snapshot,
        &active_plan,
        proposed_estimate,
        remaining_budget,
        loop_evaluation,
        now,
    )) {
        return finalize_service_replan_stop(
            &repository,
            scope,
            &snapshot,
            waiting_task,
            amendment_digest,
            reason,
            Some(&request.amendment.reason),
        )
        .await;
    }
    let write = repository
        .append_amendment(
            scope,
            snapshot.run.run_uid,
            request.expected_plan_revision,
            ValidatedAmendment {
                amendment: request.amendment,
                amendment_hash: amendment_digest,
                active_plan,
                requirement_mapping,
                superseded_task_id: waiting_task.task_id,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(match write {
        AmendmentWrite::Applied { commit, metrics } => {
            record_applied_execution_mutation(&metrics);
            applied_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
        }
        AmendmentWrite::Replayed(commit) => {
            replayed_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
        }
        AmendmentWrite::NotFound => not_found_mutation(),
        AmendmentWrite::Conflict => {
            conflict_mutation(ExecutionConflictReason::PlanRevisionMismatch)
        }
    })
}

async fn finalize_service_replan_stop(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    waiting_task: &ExecutionTaskProjection,
    amendment_digest: ExecutionHash,
    reason: moa_execution::ReplanStopReason,
    detail: Option<&str>,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let mut evaluation = evaluate_completion(CompletionEvaluationRequest {
        goal: snapshot.run.goal.clone(),
        plan: snapshot.run.active_plan.clone(),
        run_input: snapshot.run.input.clone(),
        projection: snapshot.projection.clone(),
        terminal_output: snapshot.run.output.clone(),
        budget_ledger: snapshot.budget_ledger.clone(),
        now: chrono::Utc::now(),
    })
    .map_err(execution_error)?;
    evaluation.status = replan_stop_status(
        snapshot.run.output.is_some(),
        evaluation.satisfied_requirement_ids.len(),
    );
    let stop_gaps = replan_stop_gaps(reason, detail);
    evaluation.gaps.extend(stop_gaps.iter().cloned());
    evaluation.gaps.sort();
    evaluation.gaps.dedup();
    let terminal =
        terminal_projection_from_evaluation(&evaluation, snapshot.run.output.clone(), None);
    let terminal_evidence = terminal_evidence_from_evaluation(
        ExecutionTerminalCause::ReplanStop { reason },
        &evaluation,
    )
    .map_err(execution_error)?;
    let terminal_reason =
        execution_terminal_reason(&terminal_evidence.cause, &terminal, &evaluation)
            .map_err(execution_error)?;
    let cancellation_reason = stop_gaps
        .first()
        .cloned()
        .ok_or_else(|| invalid_execution_request("replan stop omitted typed gap evidence"))?;
    let finalized = repository
        .finalize_replan_stop(
            scope,
            ReplanStopRequest {
                run_uid: snapshot.run.run_uid,
                expected_revision: snapshot.run.plan_revision,
                expected_wake_epoch: snapshot.run.wake_epoch,
                task_id: waiting_task.task_id,
                expected_generation: waiting_task.generation,
                amendment_hash: Some(amendment_digest),
                cancellation_reason,
                terminal_projection: terminal,
                completion_evaluation: evaluation,
                terminal_evidence,
                terminal_reason,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(match finalized {
        ReplanStopOutcome::Finalized(finalized) => {
            record_applied_run_transition(Some(snapshot.run.status), &finalized.run);
            applied_mutation(&finalized.run).with_task_ids_to_release(finalized.task_ids_to_release)
        }
        ReplanStopOutcome::Replayed(finalized) => replayed_mutation(&finalized.run)
            .with_task_ids_to_release(finalized.task_ids_to_release),
        ReplanStopOutcome::Conflict => {
            conflict_mutation(ExecutionConflictReason::PlanRevisionMismatch)
        }
        ReplanStopOutcome::NotFound => not_found_mutation(),
    })
}

#[cfg(test)]
/// Applies an amendment through the production inner boundary for library regressions.
pub(crate) async fn apply_amendment_for_test(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionAmendmentRequest,
) -> Result<ExecutionMutationResponse, HandlerError> {
    apply_amendment_inner(pool, config, request)
        .await
        .map(ExecutionMutationAccepted::into_response)
}

fn replan_evaluation_request(
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    proposed_plan: &moa_execution::compiler::CanonicalExecutionPlan,
    proposed_estimate: ExecutionEstimate,
    remaining_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
    loop_evaluation: ReplanLoopEvaluationRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> ReplanEvaluationRequest {
    let mut seen_plan_hashes = BTreeSet::from([
        snapshot.run.initial_plan_hash,
        snapshot.run.active_plan_hash,
    ]);
    for entry in &snapshot.run.plan_history {
        if let Some(value) = entry.get("active_plan_hash").and_then(Value::as_str)
            && let Ok(hash) = value.parse()
        {
            seen_plan_hashes.insert(hash);
        }
    }
    ReplanEvaluationRequest {
        now,
        remaining_budget,
        proposed_estimate,
        proposed_plan_hash: proposed_plan.plan_hash,
        proposed_amendment_fingerprint: loop_evaluation.proposed_amendment_fingerprint,
        seen_plan_hashes,
        seen_amendment_fingerprints: loop_evaluation.seen_amendment_fingerprints,
        failure_fingerprint_counts: loop_evaluation.failure_fingerprint_counts,
        current_failure: loop_evaluation.current_failure,
        unresolved_requirement_ids: loop_evaluation.unresolved_requirement_ids,
        amendment: loop_evaluation.amendment,
        config: loop_evaluation.config,
    }
}

fn replan_loop_evaluation_request(
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    proposed_amendment_fingerprint: ExecutionHash,
    amendment: PlanAmendment,
    config: ExecutionConfig,
    waiting_task: &ExecutionTaskProjection,
) -> moa_execution::Result<ReplanLoopEvaluationRequest> {
    let seen_amendment_fingerprints =
        durable_amendment_operation_fingerprints(&snapshot.run.plan_history)?;
    let failures = snapshot
        .projection
        .tasks
        .iter()
        .filter(|task| task.task_id != waiting_task.task_id)
        .filter_map(task_failure_fingerprint)
        .collect::<Vec<_>>();
    let current_failure = task_failure_fingerprint(waiting_task);
    let mut failure_fingerprint_counts =
        durable_failure_fingerprint_counts(&snapshot.run.plan_history);
    for failure in failures {
        if let Ok(fingerprint) = failure_fingerprint(&failure) {
            *failure_fingerprint_counts.entry(fingerprint).or_insert(0) += 1;
        }
    }
    let unresolved_requirement_ids = snapshot
        .run
        .goal
        .requirements
        .iter()
        .filter(|requirement| {
            !snapshot
                .run
                .active_plan
                .definition
                .nodes
                .iter()
                .any(|node| {
                    node.requirement_ids.contains(&requirement.id)
                        && snapshot.projection.node_statuses.get(&node.id)
                            == Some(&moa_execution::state::ExecutionNodeStatus::Completed)
                })
        })
        .map(|requirement| requirement.id.clone())
        .collect();
    Ok(ReplanLoopEvaluationRequest {
        proposed_amendment_fingerprint,
        seen_amendment_fingerprints,
        failure_fingerprint_counts,
        current_failure,
        unresolved_requirement_ids,
        amendment,
        config,
    })
}

fn durable_amendment_operation_fingerprints(
    plan_history: &[Value],
) -> moa_execution::Result<BTreeSet<ExecutionHash>> {
    let mut fingerprints = BTreeSet::new();
    for entry in plan_history {
        let Some(amendment) = entry.get("amendment") else {
            continue;
        };
        let amendment =
            serde_json::from_value::<PlanAmendment>(amendment.clone()).map_err(|error| {
                moa_execution::Error::InvalidRepositoryData {
                    message: format!(
                        "persisted plan history contains an invalid amendment: {error}"
                    ),
                }
            })?;
        fingerprints.insert(amendment_operations_fingerprint(&amendment)?);
    }
    Ok(fingerprints)
}

fn durable_failure_fingerprint_counts(plan_history: &[Value]) -> BTreeMap<ExecutionHash, u32> {
    let mut counts: BTreeMap<ExecutionHash, u32> = BTreeMap::new();
    for entry in plan_history {
        let Some(fingerprint) = entry
            .get("failure_fingerprint")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<ExecutionHash>().ok())
        else {
            continue;
        };
        let count = entry
            .get("failure_fingerprint_count")
            .and_then(Value::as_u64)
            .map_or(1, |count| u32::try_from(count).unwrap_or(u32::MAX));
        counts
            .entry(fingerprint)
            .and_modify(|persisted| *persisted = (*persisted).max(count))
            .or_insert(count);
    }
    counts
}

fn task_failure_fingerprint(task: &ExecutionTaskProjection) -> Option<FailureFingerprintInput> {
    let outcome = task.outcome.as_ref()?;
    let (class, message) = match &outcome.result {
        ExecutionTaskResult::Failed { class, message } => (class.clone(), message.clone()),
        ExecutionTaskResult::NeedsReplan { reason, .. } => {
            (ExecutionFailureClass::Terminal, reason.clone())
        }
        ExecutionTaskResult::Completed { .. }
        | ExecutionTaskResult::NeedsInput { .. }
        | ExecutionTaskResult::Cancelled { .. } => return None,
    };
    Some(FailureFingerprintInput {
        class,
        node_id: task.node_id.clone(),
        capability_ref: None,
        message,
    })
}

async fn mutation_from_transition(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
    transition: TransitionOutcome,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    match transition {
        TransitionOutcome::Applied(task) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            let accepted = applied_mutation(&run);
            Ok(if task.status.is_terminal() {
                accepted.with_task_ids_to_release(vec![task.task_id])
            } else {
                accepted
            })
        }
        TransitionOutcome::RunApplied(_) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            Ok(applied_mutation(&run))
        }
        TransitionOutcome::AlreadyApplied(task) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            let accepted = replayed_mutation(&run);
            Ok(if task.status.is_terminal() {
                accepted.with_task_ids_to_release(vec![task.task_id])
            } else {
                accepted
            })
        }
        TransitionOutcome::RunAlreadyApplied(_) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            Ok(replayed_mutation(&run))
        }
        TransitionOutcome::NotFound => Ok(not_found_mutation()),
        TransitionOutcome::Rejected(reason) => Ok(conflict_mutation(match reason {
            TransitionRejection::GenerationMismatch => ExecutionConflictReason::GenerationMismatch,
            TransitionRejection::InvalidTaskStatus
            | TransitionRejection::InvalidRunStatus
            | TransitionRejection::DeadlineElapsed
            | TransitionRejection::BudgetExceeded => ExecutionConflictReason::InvalidStatus,
            TransitionRejection::CounterOverflow => ExecutionConflictReason::AlreadyTerminal,
        })),
    }
}

fn mutation_from_task_write(write: TaskOutcomeWrite) -> ExecutionMutationAccepted {
    match write {
        TaskOutcomeWrite::Applied { run, .. } => applied_mutation(&run),
        TaskOutcomeWrite::Replayed { run, .. } => replayed_mutation(&run),
        TaskOutcomeWrite::Rejected { reason, .. } => {
            use moa_execution::repository::TaskOutcomeRejection;
            conflict_mutation(match reason {
                TaskOutcomeRejection::StaleGeneration => {
                    ExecutionConflictReason::GenerationMismatch
                }
                TaskOutcomeRejection::TerminalTask | TaskOutcomeRejection::TerminalRun => {
                    ExecutionConflictReason::AlreadyTerminal
                }
                TaskOutcomeRejection::InvalidTaskStatus
                | TaskOutcomeRejection::NonCumulativeUsage
                | TaskOutcomeRejection::UnsupportedSchemaVersion => {
                    ExecutionConflictReason::InvalidStatus
                }
            })
        }
        TaskOutcomeWrite::NotFound => not_found_mutation(),
    }
}

fn execution_scope(
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<moa_core::types::contact::ContactId>,
) -> ExecutionScope {
    contact_id.map_or(ExecutionScope::Tenant { tenant_id }, |contact_id| {
        ExecutionScope::Contact {
            tenant_id,
            contact_id,
        }
    })
}

fn verify_run_request(
    run: &ExecutionRunRecord,
    request: &ExecutionRunRequest,
) -> Result<(), HandlerError> {
    verify_run_scope(
        run,
        request.tenant_id,
        request.contact_id,
        request.session_id,
    )
}

fn verify_run_scope(
    run: &ExecutionRunRecord,
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<moa_core::types::contact::ContactId>,
    session_id: moa_core::types::identifiers::SessionId,
) -> Result<(), HandlerError> {
    if run.tenant_id == tenant_id && run.contact_id == contact_id && run.session_id == session_id {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(409, "execution scope mismatch").into())
    }
}

fn verify_start_replay(
    run: &ExecutionRunRecord,
    request: &ExecutionStartRequest,
    snapshot: &ExecutionPlanningContextSnapshot,
) -> Result<(), HandlerError> {
    let expected_hash = request
        .planning_context_hash
        .parse::<ExecutionHash>()
        .map_err(execution_error)?;
    if run.originating_user_sequence_num != request.originating_user_sequence_num
        || run.planning_context_uid != request.planning_context_uid
        || run.planning_context_hash != expected_hash
        || run.owner_user_id != snapshot.owner_user_id
        || run.goal != request.compiled.goal
        || run.initial_plan != request.compiled.plan
        || run.catalog != snapshot.catalog
        || run.authorization != snapshot.authorization
        || run.pinned_instruction_skills != snapshot.pinned_instruction_skills
        || run.source_provenance != request.source_provenance
        || run.input != request.run_input
        || run.approved_budget != snapshot.budget
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution start idempotency key conflicts with immutable admission input",
        )
        .into());
    }
    Ok(())
}

fn run_summary(run: &ExecutionRunRecord) -> ExecutionRunSummary {
    ExecutionRunSummary {
        run_uid: run.run_uid,
        session_id: run.session_id,
        originating_user_sequence_num: run.originating_user_sequence_num,
        status: run.status,
        source_kind: run.source_kind,
        skill_template_ref: run.skill_template_ref.clone(),
        skill_template_revision_uid: run.skill_template_revision_uid,
        plan_revision: run.plan_revision,
        total_tasks: run.progress_total_tasks,
        completed_tasks: run.progress_completed_tasks,
        failed_tasks: run.progress_failed_tasks,
        budget_ledger: BudgetLedger {
            limit: run.approved_budget.clone(),
            reserved: run.reserved,
            consumed: run.consumed,
            overrun: run.budget_overrun,
        },
        created_at: run.created_at,
        queued_at: run.queued_at,
        updated_at: run.updated_at,
        completed_at: run.completed_at,
        terminal_evidence: run.terminal_evidence.clone(),
        terminal_reason: run.terminal_reason,
    }
}

fn task_projection(task: &ExecutionTaskRecord) -> ExecutionTaskProjection {
    ExecutionTaskProjection {
        task_id: task.task_id,
        node_id: task.node_id.clone(),
        item_key: task.item_key.clone(),
        status: task.status,
        attempt: task.attempt,
        generation: task.generation,
        input: task.input.clone(),
        outcome: task.current_outcome.clone(),
    }
}

fn applied_mutation(run: &ExecutionRunRecord) -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Accepted {
        response: ExecutionMutationResponse::Applied {
            run: run_summary(run),
        },
        handoff: ExecutionMutationHandoff {
            wake_epoch: run.wake_epoch,
            task_ids_to_release: Vec::new(),
        },
    }
}

fn replayed_mutation(run: &ExecutionRunRecord) -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Accepted {
        response: ExecutionMutationResponse::Replayed {
            run: run_summary(run),
        },
        handoff: ExecutionMutationHandoff {
            wake_epoch: run.wake_epoch,
            task_ids_to_release: Vec::new(),
        },
    }
}

fn conflict_mutation(reason: ExecutionConflictReason) -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Rejected {
        response: ExecutionMutationResponse::Conflict { reason },
    }
}

fn not_found_mutation() -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Rejected {
        response: ExecutionMutationResponse::NotFound,
    }
}

fn zero_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

fn execution_run_started_delivery(
    response: &ExecutionStartResponse,
) -> ExecutionRunStartedDelivery {
    let status = if response.confirmation_required {
        ExecutionRunAdmissionStatus::AwaitingConfirmation
    } else {
        ExecutionRunAdmissionStatus::Queued
    };
    let confirmation = response
        .confirmation_required
        .then(|| ExecutionConfirmationEvidence {
            active_plan_hash: response.active_plan_hash.to_string(),
            estimate: ExecutionAdmissionEstimate {
                cost_microusd: response.estimate.cost_microusd,
                tokens: response.estimate.tokens,
                tasks: response.estimate.tasks,
                tool_calls: response.estimate.tool_calls,
                retrieved_bytes: response.estimate.retrieved_bytes,
            },
            methodology: ExecutionEstimateMethodology::ConservativeWorstCase,
        });
    ExecutionRunStartedDelivery {
        started: ExecutionRunStarted {
            run_uid: response.run.run_uid,
            originating_user_sequence_num: response.run.originating_user_sequence_num,
            plan_revision: response.run.plan_revision,
            status,
            confirmation,
        },
        approved_budget: response.run.budget_ledger.limit.clone(),
    }
}

fn send_run_wake(
    ctx: &Context<'_>,
    run_uid: uuid::Uuid,
    wake_epoch: u64,
    reason: ExecutionRunWakeReason,
) {
    crate::restate_identity::replay_safe_request(
        ctx.workflow_client::<ExecutionRunClient>(run_uid.to_string())
            .wake(Json::from(ExecutionRunWakeRequest {
                run_uid,
                wake_epoch,
                reason,
            })),
    )
    .send();
}

fn invalid_execution_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

fn execution_error(error: moa_execution::Error) -> HandlerError {
    match error {
        moa_execution::Error::Storage { message } => {
            TerminalError::new_with_code(503, format!("execution storage unavailable: {message}"))
                .into()
        }
        other => invalid_execution_request(other.to_string()),
    }
}

async fn list_capabilities_inner(
    pool: sqlx::PgPool,
    registrations: Vec<(ToolDefinition, ToolExecution)>,
    request: CapabilitiesListRequest,
) -> Result<CapabilitiesListResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(pool.clone());
    let revisions = load_published_revisions(&registry, &scope)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let connection_refs = load_connection_refs(pool, request.tenant_id)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    build_capability_response(&registrations, &revisions, &connection_refs).map_err(|error| {
        TerminalError::new(format!(
            "failed to build execution capability catalog: {error}"
        ))
        .into()
    })
}

pub(crate) fn build_capability_response(
    registrations: &[(ToolDefinition, ToolExecution)],
    revisions: &[StoredArtifactRevision],
    connection_refs: &[String],
) -> moa_execution::Result<CapabilitiesListResponse> {
    let registered = registrations
        .iter()
        .map(|(definition, execution)| {
            (
                definition.name.clone(),
                (definition.clone(), execution.clone()),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut capabilities = registrations
        .iter()
        .map(|(definition, execution)| registered_tool_capability(definition, execution))
        .collect::<moa_execution::Result<Vec<_>>>()?;
    let mut diagnostics = connection_refs
        .iter()
        .map(|reference| CapabilityCatalogDiagnostic {
            code: CapabilityCatalogDiagnosticCode::ConnectionOnlyDataSource,
            reference: reference.clone(),
            message:
                "knowledge connections configure data access but have no typed invocation owner"
                    .to_string(),
        })
        .collect::<Vec<_>>();
    let mut artifact_tools = HashMap::new();

    for revision in revisions {
        match &revision.document.definition {
            ArtifactDefinition::Action(action) => {
                let action_ref = ArtifactRef::action_artifact(revision.name.clone());
                match resolve_tool(action.tool_name.as_deref(), &registered) {
                    Some((definition, execution)) => {
                        artifact_tools.insert(action_ref.to_string(), definition.name.clone());
                        capabilities.push(action_capability(ActionCapabilityRequest {
                            action_ref,
                            revision_uid: revision.revision_uid,
                            description: &action.description,
                            input_schema: &action.input_schema,
                            output_schema: &action.output_schema,
                            admin_review_required: action.admin_review_required,
                            definition,
                            execution,
                        }));
                    }
                    None => diagnostics.push(unresolved_action_diagnostic(
                        action_ref.to_string(),
                        action.tool_name.as_deref(),
                    )),
                }
            }
            ArtifactDefinition::Connector(connector) => {
                for action in &connector.actions {
                    let action_ref = ArtifactRef::action(revision.name.clone(), action.id.clone());
                    let connector_ref = ArtifactRef::connector(revision.name.clone());
                    match resolve_tool(action.tool_name.as_deref(), &registered) {
                        Some((definition, execution)) => {
                            artifact_tools.insert(action_ref.to_string(), definition.name.clone());
                            capabilities.push(connector_action_capability(
                                action_ref,
                                connector_ref,
                                revision.revision_uid,
                                action,
                                definition,
                                execution,
                            ));
                        }
                        None => diagnostics.push(unresolved_action_diagnostic(
                            action_ref.to_string(),
                            action.tool_name.as_deref(),
                        )),
                    }
                }
            }
            ArtifactDefinition::Skill(_)
            | ArtifactDefinition::Agent(_)
            | ArtifactDefinition::ExperimentPlan(_) => {}
        }
    }

    for revision in revisions {
        let ArtifactDefinition::Skill(skill) = &revision.document.definition else {
            continue;
        };
        let skill_ref = ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone());
        for action in &skill.actions {
            append_skill_action(SkillActionContext {
                capabilities: &mut capabilities,
                diagnostics: &mut diagnostics,
                registered: &registered,
                artifact_tools: &artifact_tools,
                skill_ref: skill_ref.clone(),
                revision_uid: revision.revision_uid,
                action,
            });
        }
    }

    diagnostics.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(CapabilitiesListResponse {
        catalog: ExecutionCapabilityCatalog::build(capabilities)?,
        diagnostics,
    })
}

/// Exact governed catalog and allowlist supplied to skill-regression compilation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SkillRegressionCompileAuthority {
    /// Tenant-visible catalog built by the production execution catalog builder.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact capability and skill references authorized for this candidate review.
    pub authorization: moa_execution::ExecutionAuthorizationEnvelope,
}

/// Resolves compiler authority for one exact draft skill under its review scope.
pub(crate) async fn resolve_skill_regression_compile_authority(
    pool: sqlx::PgPool,
    registrations: Vec<(ToolDefinition, ToolExecution)>,
    scope: ActionRuleScope,
    draft: StoredArtifactRevision,
) -> MoaResult<SkillRegressionCompileAuthority> {
    if draft.kind != ArtifactKind::Skill || draft.status != ArtifactStatus::Draft {
        return Err(MoaError::ValidationError(
            "skill regression authority requires the exact draft skill revision".to_string(),
        ));
    }

    let registry = ArtifactRegistry::new(pool.clone());
    let mut revisions = load_published_revisions(&registry, &scope).await?;
    let connection_refs = load_connection_refs(pool, scope.tenant_id()).await?;
    revisions.push(draft);
    build_skill_regression_compile_authority(&registrations, &revisions, &connection_refs)
}

fn build_skill_regression_compile_authority(
    registrations: &[(ToolDefinition, ToolExecution)],
    revisions: &[StoredArtifactRevision],
    connection_refs: &[String],
) -> MoaResult<SkillRegressionCompileAuthority> {
    let response = build_capability_response(registrations, revisions, connection_refs)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;

    let mut skill_refs = revisions
        .iter()
        .filter(|revision| matches!(revision.document.definition, ArtifactDefinition::Skill(_)))
        .map(|revision| ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone()))
        .collect::<Vec<_>>();
    skill_refs.sort_by_key(ToString::to_string);
    skill_refs.dedup();
    let authorization = moa_execution::ExecutionAuthorizationEnvelope {
        capability_refs: response
            .catalog
            .capabilities
            .iter()
            .map(|capability| capability.reference.clone())
            .collect(),
        skill_refs,
    };

    Ok(SkillRegressionCompileAuthority {
        catalog: response.catalog,
        authorization,
    })
}

fn registered_tool_capability(
    definition: &ToolDefinition,
    execution: &ToolExecution,
) -> moa_execution::Result<ExecutionCapability> {
    let (source, execution_class, domain, owner) = match execution {
        ToolExecution::BuiltIn(_) if definition.name.starts_with("memory_") => (
            CapabilitySource::Memory {
                operation: definition
                    .name
                    .strip_prefix("memory_")
                    .unwrap_or(definition.name.as_str())
                    .to_string(),
                tool_name: definition.name.clone(),
            },
            ExecutionClass::Data,
            "moa.execution.capability.memory",
            json!({"kind": "memory"}),
        ),
        ToolExecution::BuiltIn(_) => (
            CapabilitySource::BuiltInTool {
                name: definition.name.clone(),
            },
            if definition.policy.action_class == moa_core::types::action_policy::ActionClass::Read {
                ExecutionClass::Data
            } else {
                ExecutionClass::Compute
            },
            "moa.execution.capability.builtin",
            json!({"kind": "builtin"}),
        ),
        ToolExecution::Hand { .. } => (
            CapabilitySource::HandTool {
                name: definition.name.clone(),
            },
            ExecutionClass::Compute,
            "moa.execution.capability.hand",
            json!({"kind": "hand"}),
        ),
        ToolExecution::Mcp { server_name } => (
            CapabilitySource::McpTool {
                server: server_name.clone(),
                name: definition.name.clone(),
            },
            ExecutionClass::External,
            "moa.execution.capability.mcp",
            json!({"kind": "mcp", "server": server_name}),
        ),
    };
    let version = capability_version(
        domain,
        &json!({
            "name": definition.name,
            "input_schema": definition.schema,
            "policy": definition.policy,
            "idempotency_class": definition.idempotency_class,
            "max_output_tokens": definition.max_output_tokens,
            "owner": owner,
        }),
    )?;
    Ok(ExecutionCapability {
        reference: CapabilityReference {
            name: definition.name.clone(),
            version,
        },
        description: definition.description.clone(),
        input_schema: definition.schema.clone(),
        output_schema: generic_json_output_schema(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: definition.policy.default_effect,
        idempotency_class: definition.idempotency_class,
        execution_class,
        source,
        estimate: single_tool_estimate(definition.max_output_tokens),
    })
}

struct ActionCapabilityRequest<'a> {
    action_ref: ArtifactRef,
    revision_uid: uuid::Uuid,
    description: &'a str,
    input_schema: &'a Value,
    output_schema: &'a Value,
    admin_review_required: bool,
    definition: &'a ToolDefinition,
    execution: &'a ToolExecution,
}

fn action_capability(request: ActionCapabilityRequest<'_>) -> ExecutionCapability {
    let ActionCapabilityRequest {
        action_ref,
        revision_uid,
        description,
        input_schema,
        output_schema,
        admin_review_required,
        definition,
        execution,
    } = request;
    ExecutionCapability {
        reference: CapabilityReference {
            name: action_ref.to_string(),
            version: revision_uid.to_string(),
        },
        description: description.to_string(),
        input_schema: input_schema.clone(),
        output_schema: output_schema.clone(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: artifact_effect(admin_review_required, &definition.policy),
        idempotency_class: definition.idempotency_class,
        execution_class: execution_class(execution, definition),
        source: CapabilitySource::ActionArtifact {
            action_ref,
            revision_uid,
            tool_name: definition.name.clone(),
        },
        estimate: single_tool_estimate(definition.max_output_tokens),
    }
}

fn connector_action_capability(
    action_ref: ArtifactRef,
    connector_ref: ArtifactRef,
    revision_uid: uuid::Uuid,
    action: &moa_artifacts::connector::ConnectorActionDefinition,
    definition: &ToolDefinition,
    execution: &ToolExecution,
) -> ExecutionCapability {
    ExecutionCapability {
        reference: CapabilityReference {
            name: action_ref.to_string(),
            version: revision_uid.to_string(),
        },
        description: action.description.clone(),
        input_schema: action.input_schema.clone(),
        output_schema: action.output_schema.clone(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: artifact_effect(action.admin_review_required, &definition.policy),
        idempotency_class: definition.idempotency_class,
        execution_class: execution_class(execution, definition),
        source: CapabilitySource::ConnectorAction {
            connector_ref,
            revision_uid,
            action_id: action.id.clone(),
            tool_name: definition.name.clone(),
        },
        estimate: single_tool_estimate(definition.max_output_tokens),
    }
}

struct SkillActionContext<'a> {
    capabilities: &'a mut Vec<ExecutionCapability>,
    diagnostics: &'a mut Vec<CapabilityCatalogDiagnostic>,
    registered: &'a HashMap<String, (ToolDefinition, ToolExecution)>,
    artifact_tools: &'a HashMap<String, String>,
    skill_ref: ArtifactRef,
    revision_uid: uuid::Uuid,
    action: &'a SkillActionDefinition,
}

fn append_skill_action(context: SkillActionContext<'_>) {
    let SkillActionContext {
        capabilities,
        diagnostics,
        registered,
        artifact_tools,
        skill_ref,
        revision_uid,
        action,
    } = context;
    let reference = format!("{skill_ref}#{}", action.id);
    if action.kind == SkillActionKind::Code {
        diagnostics.push(CapabilityCatalogDiagnostic {
            code: CapabilityCatalogDiagnosticCode::UnownedSkillCode,
            reference,
            message: "skill code has no registered typed execution owner".to_string(),
        });
        return;
    }
    let tool_name = action.artifact_ref.as_ref().and_then(|artifact_ref| {
        artifact_tools
            .get(&artifact_ref.to_string())
            .cloned()
            .or_else(|| match artifact_ref {
                ArtifactRef::Tool { name } => Some(name.clone()),
                ArtifactRef::Artifact { .. } | ArtifactRef::Action { .. } => None,
            })
    });
    let Some((definition, execution)) = resolve_tool(tool_name.as_deref(), registered) else {
        diagnostics.push(unresolved_action_diagnostic(
            reference,
            tool_name.as_deref(),
        ));
        return;
    };
    capabilities.push(ExecutionCapability {
        reference: CapabilityReference {
            name: reference,
            version: revision_uid.to_string(),
        },
        description: action.description.clone(),
        input_schema: action.input_schema.clone(),
        output_schema: action.output_schema.clone(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: definition.policy.default_effect,
        idempotency_class: definition.idempotency_class,
        execution_class: execution_class(execution, definition),
        source: CapabilitySource::SkillAction {
            skill_ref,
            revision_uid,
            action_id: action.id.clone(),
            tool_name: definition.name.clone(),
        },
        estimate: single_tool_estimate(definition.max_output_tokens),
    });
}

fn resolve_tool<'a>(
    tool_name: Option<&str>,
    registered: &'a HashMap<String, (ToolDefinition, ToolExecution)>,
) -> Option<(&'a ToolDefinition, &'a ToolExecution)> {
    let (definition, execution) = registered.get(tool_name?)?;
    Some((definition, execution))
}

fn unresolved_action_diagnostic(
    reference: String,
    tool_name: Option<&str>,
) -> CapabilityCatalogDiagnostic {
    CapabilityCatalogDiagnostic {
        code: CapabilityCatalogDiagnosticCode::UnresolvedActionTool,
        reference,
        message: tool_name.map_or_else(
            || "action does not declare a backing tool".to_string(),
            |name| format!("action backing tool `{name}` is not registered"),
        ),
    }
}

fn artifact_effect(admin_review_required: bool, policy: &ToolPolicySpec) -> ActionPolicyEffect {
    if admin_review_required {
        ActionPolicyEffect::AdminReview
    } else {
        policy.default_effect
    }
}

fn execution_class(execution: &ToolExecution, definition: &ToolDefinition) -> ExecutionClass {
    match execution {
        ToolExecution::Hand { .. } => ExecutionClass::Compute,
        ToolExecution::Mcp { .. } => ExecutionClass::External,
        ToolExecution::BuiltIn(_)
            if definition.policy.action_class
                == moa_core::types::action_policy::ActionClass::Read =>
        {
            ExecutionClass::Data
        }
        ToolExecution::BuiltIn(_) => ExecutionClass::Compute,
    }
}

fn single_tool_estimate(max_output_tokens: u32) -> ExecutionEstimate {
    ExecutionEstimate {
        tool_calls: 1,
        tasks: 1,
        // Tool output budgeting uses a conservative four-characters-per-token
        // approximation. UTF-8 and JSON escaping can expand each character to
        // at most four bytes in the structured payload retained for execution.
        retrieved_bytes: u64::from(max_output_tokens).saturating_mul(16).max(4),
        ..ExecutionEstimate::default()
    }
}

fn generic_json_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "description": "The complete JSON value returned by the registered tool."
    })
}

async fn load_published_revisions(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
) -> MoaResult<Vec<StoredArtifactRevision>> {
    let mut revisions = Vec::new();
    for kind in [
        ArtifactKind::Action,
        ArtifactKind::Connector,
        ArtifactKind::Skill,
    ] {
        let summaries = registry
            .list_visible(scope, Some(kind), Some(ArtifactStatus::Published))
            .await?;
        for summary in summaries {
            if let Some(revision) = registry.load_revision(scope, summary.revision_uid).await? {
                revisions.push(revision);
            }
        }
    }
    Ok(revisions)
}

async fn load_locked_skill_revisions(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    agent_context: Option<&moa_core::types::agent::AgentContext>,
) -> Result<Vec<StoredArtifactRevision>, HandlerError> {
    let Some(agent_context) = agent_context else {
        return Ok(Vec::new());
    };
    let mut dependencies = agent_context
        .artifact_dependencies
        .iter()
        .filter(|dependency| dependency.kind == "skill")
        .collect::<Vec<_>>();
    dependencies.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });

    let mut revisions = Vec::with_capacity(dependencies.len());
    for dependency in dependencies {
        let canonical_ref =
            canonical_skill_policy_ref(&dependency.reference).map_err(invalid_execution_request)?;
        let revision = registry
            .load_revision(scope, dependency.revision_uid)
            .await
            .map_err(moa_error_to_status_handler_error)?
            .ok_or_else(|| {
                invalid_execution_request(format!(
                    "session skill lock revision is not visible: {}",
                    dependency.revision_uid
                ))
            })?;
        let revision_ref = skill_revision_ref(&revision).map_err(invalid_execution_request)?;
        if revision_ref != canonical_ref
            || revision.name != dependency.name
            || revision.artifact_uid != dependency.artifact_uid
            || revision.revision_uid != dependency.revision_uid
            || revision.version != dependency.version
        {
            return Err(invalid_execution_request(format!(
                "session skill lock does not match persisted revision: {}",
                dependency.reference
            )));
        }
        revisions.push(revision);
    }
    Ok(revisions)
}

async fn load_connection_refs(
    pool: sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> MoaResult<Vec<String>> {
    PostgresKnowledgeRepository::scoped(pool, RlsContext::tenant(tenant_id))
        .list_connections(tenant_id, None)
        .await
        .map(|connections| {
            connections
                .into_iter()
                .map(|projection| projection.connection.connection_uid.to_string())
                .collect()
        })
        .map_err(|error| {
            tracing::error!(error = %error, "execution capability connection listing failed");
            MoaError::StorageError("failed to inspect knowledge connections".to_string())
        })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::Utc;
    use moa_artifacts::document::{
        ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus,
    };
    use moa_artifacts::execution_plan::{
        ExecutionGoalTemplate, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
        ExecutionPlanTemplate, PlanAmendment, PlanAmendmentOperation, RetryPolicy,
    };
    use moa_artifacts::registry::StoredArtifactRevision;
    use moa_artifacts::skill::SkillDefinition;
    use moa_core::types::{
        agent::{AgentSkillPolicy, AgentSkillPolicyMode},
        execution_planning::{
            ExecutionPlanningContractError, ExecutionSourceProvenance, PinnedExecutionTemplateRef,
        },
    };
    use moa_execution::{
        CapabilityCatalogDiagnosticCode, CapabilitySource, ExecutionClass,
        capability::{amendment_hash, amendment_operations_fingerprint},
        replan::{
            ReplanDecision, ReplanLoopEvaluationRequest, ReplanStopReason,
            evaluate_replan_loop_stop, failure_fingerprint,
        },
        state::FailureFingerprintInput,
        wire::PinnedExecutionTemplate,
    };
    use moa_hands::{McpDiscoveredTool, ToolRegistry};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{
        build_capability_response, build_planning_skill_context,
        build_skill_regression_compile_authority, durable_amendment_operation_fingerprints,
        durable_failure_fingerprint_counts, persisted_input_audience, single_tool_estimate,
        validate_external_wait_payload, validate_start_source_provenance,
    };

    #[test]
    fn tool_estimate_reserves_serialized_output_bytes_from_token_budget() {
        // Pins: a successful non-empty tool result cannot overrun a zero-byte reservation
        // before dependent reducer/output tasks have a chance to run.
        assert_eq!(single_tool_estimate(0).retrieved_bytes, 4);
        assert_eq!(single_tool_estimate(4_000).retrieved_bytes, 64_000);
        assert_eq!(
            single_tool_estimate(u32::MAX).retrieved_bytes,
            u64::from(u32::MAX) * 16
        );
    }

    fn revision(
        name: &str,
        revision_uid: u128,
        document: ArtifactDocument,
    ) -> StoredArtifactRevision {
        StoredArtifactRevision {
            artifact_uid: Uuid::from_u128(revision_uid + 100),
            revision_uid: Uuid::from_u128(revision_uid),
            storage_partition_id: None,
            user_id: None,
            scope: "tenant".to_string(),
            kind: document.kind.clone(),
            name: name.to_string(),
            description: document.metadata.description.clone(),
            tags: Vec::new(),
            document,
            canonical_hash: vec![1],
            source_format: "json".to_string(),
            source_text: Vec::new(),
            status: ArtifactStatus::Published,
            validation_report: json!({}),
            version: 1,
            published_at: Some(Utc::now()),
            valid_to: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn document(
        name: &str,
        kind: ArtifactKind,
        definition: ArtifactDefinition,
    ) -> ArtifactDocument {
        serde_json::from_value(json!({
            "api_version": "moa/v1",
            "kind": kind,
            "metadata": {"name": name, "description": format!("{name} description")},
            "status": "published",
            "definition": definition,
            "ui": {},
            "reference_resolutions": []
        }))
        .expect("artifact fixture should decode")
    }

    fn skill_revision(name: &str, revision_uid: u128) -> StoredArtifactRevision {
        revision(
            name,
            revision_uid,
            document(
                name,
                ArtifactKind::Skill,
                ArtifactDefinition::Skill(SkillDefinition {
                    instructions: Default::default(),
                    inputs: json!({"type": "object"}),
                    outputs: json!({"type": "object"}),
                    actions: Vec::new(),
                    connectors: Vec::new(),
                    allowed_tools: Vec::new(),
                    execution_plan: Some(ExecutionPlanTemplate {
                        goal: ExecutionGoalTemplate {
                            requirements: Vec::new(),
                            deliverables: Vec::new(),
                            coverage: Vec::new(),
                            constraints: Vec::new(),
                            completion_checks: Vec::new(),
                        },
                        plan: ExecutionPlanDefinition {
                            schema_version: 1,
                            input_schema: json!({"type": "object"}),
                            output_schema: json!({"type": "object"}),
                            nodes: Vec::new(),
                        },
                    }),
                    ui: json!({}),
                }),
            ),
        )
    }

    fn selected_skill_refs(context: &super::PlanningSkillContext) -> Vec<(String, Uuid)> {
        context
            .pinned_instruction_skills
            .iter()
            .map(|skill| (skill.skill_ref.to_string(), skill.revision_uid))
            .collect()
    }

    fn selected_revision_refs(context: &super::PlanningSkillContext) -> Vec<(String, Uuid)> {
        context
            .revisions
            .iter()
            .filter(|revision| matches!(revision.document.definition, ArtifactDefinition::Skill(_)))
            .map(|revision| (format!("skill://{}", revision.name), revision.revision_uid))
            .collect()
    }

    fn selected_template_refs(context: &super::PlanningSkillContext) -> Vec<(String, Uuid)> {
        context
            .execution_templates
            .iter()
            .map(|template| (template.skill_ref.to_string(), template.revision_uid))
            .collect()
    }

    fn assert_selected_skill_revisions(
        context: &super::PlanningSkillContext,
        expected: &[(String, Uuid)],
    ) {
        assert_eq!(selected_skill_refs(context), expected);
        assert_eq!(selected_revision_refs(context), expected);
        assert_eq!(selected_template_refs(context), expected);
    }

    #[test]
    fn planning_context_auto_uses_session_locked_revision() {
        // Pins: Auto chooses visible skills, then every matching session lock
        // substitutes the exact revision before authority/templates are derived.
        let policy = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Auto,
            refs: Vec::new(),
            max_visible: None,
        };
        let context = build_planning_skill_context(
            vec![skill_revision("alpha", 2), skill_revision("beta", 4)],
            vec![skill_revision("alpha", 1)],
            &policy,
            None,
        )
        .expect("Auto selection should accept a matching older session lock");
        assert_selected_skill_revisions(
            &context,
            &[
                ("skill://alpha".to_string(), Uuid::from_u128(1)),
                ("skill://beta".to_string(), Uuid::from_u128(4)),
            ],
        );
    }

    #[test]
    fn planning_context_denylist_substitutes_locks_without_restoring_denied_skills() {
        // Pins: Denylist substitutes locks only for selected non-denied skills;
        // a matching lock never restores a denied skill to planning authority.
        let policy = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Denylist,
            refs: vec!["skill://beta".to_string()],
            max_visible: None,
        };
        let context = build_planning_skill_context(
            vec![
                skill_revision("alpha", 2),
                skill_revision("beta", 4),
                skill_revision("gamma", 6),
            ],
            vec![
                skill_revision("alpha", 1),
                skill_revision("beta", 3),
                skill_revision("gamma", 5),
            ],
            &policy,
            None,
        )
        .expect("Denylist selection should substitute only non-denied locks");
        assert_selected_skill_revisions(
            &context,
            &[
                ("skill://alpha".to_string(), Uuid::from_u128(1)),
                ("skill://gamma".to_string(), Uuid::from_u128(5)),
            ],
        );
    }

    #[test]
    fn planning_context_lock_substitution_preserves_max_visible_and_order() {
        // Pins: max_visible and reference ordering are resolved from policy
        // selection before matching locks replace revisions deterministically.
        let policy = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Auto,
            refs: Vec::new(),
            max_visible: Some(2),
        };
        let locks = vec![
            skill_revision("gamma", 5),
            skill_revision("beta", 3),
            skill_revision("alpha", 1),
        ];
        let forward = build_planning_skill_context(
            vec![
                skill_revision("gamma", 6),
                skill_revision("alpha", 2),
                skill_revision("beta", 4),
            ],
            locks.clone(),
            &policy,
            None,
        )
        .expect("forward selection should be valid");
        let reverse = build_planning_skill_context(
            vec![
                skill_revision("beta", 4),
                skill_revision("alpha", 2),
                skill_revision("gamma", 6),
            ],
            locks,
            &policy,
            None,
        )
        .expect("reverse selection should be valid");
        let expected = [
            ("skill://alpha".to_string(), Uuid::from_u128(1)),
            ("skill://beta".to_string(), Uuid::from_u128(3)),
        ];
        assert_selected_skill_revisions(&forward, &expected);
        assert_selected_skill_revisions(&reverse, &expected);
    }

    #[test]
    fn planning_context_skill_policy_allowlist_and_denylist_never_broaden() {
        // Pins: planning authority includes only the session-pinned allowlist and excludes every denylisted skill.
        let revisions = vec![
            skill_revision("gamma", 3),
            skill_revision("alpha", 1),
            skill_revision("beta", 2),
        ];
        let allowlist = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Allowlist,
            refs: vec!["skill://beta".to_string()],
            max_visible: None,
        };
        let allowed = build_planning_skill_context(revisions.clone(), Vec::new(), &allowlist, None)
            .expect("valid allowlist should select planning skills");
        assert_eq!(
            selected_skill_refs(&allowed),
            vec![("skill://beta".to_string(), Uuid::from_u128(2))]
        );

        let denylist = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Denylist,
            refs: vec!["skill://beta".to_string()],
            max_visible: None,
        };
        let denied = build_planning_skill_context(revisions, Vec::new(), &denylist, None)
            .expect("valid denylist should select planning skills");
        assert_eq!(
            selected_skill_refs(&denied),
            vec![
                ("skill://alpha".to_string(), Uuid::from_u128(1)),
                ("skill://gamma".to_string(), Uuid::from_u128(3)),
            ]
        );
    }

    #[test]
    fn planning_context_skill_policy_max_visible_is_deterministic_and_pinned_first() {
        // Pins: max_visible selection is input-order independent and reserves capacity for pinned refs.
        let policy = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Pinned,
            refs: vec!["skill://gamma".to_string()],
            max_visible: Some(2),
        };
        let forward = build_planning_skill_context(
            vec![
                skill_revision("gamma", 3),
                skill_revision("alpha", 1),
                skill_revision("beta", 2),
            ],
            Vec::new(),
            &policy,
            None,
        )
        .expect("pinned policy should select planning skills");
        let reverse = build_planning_skill_context(
            vec![
                skill_revision("beta", 2),
                skill_revision("alpha", 1),
                skill_revision("gamma", 3),
            ],
            Vec::new(),
            &policy,
            None,
        )
        .expect("reordered input should select the same planning skills");
        let expected = vec![
            ("skill://alpha".to_string(), Uuid::from_u128(1)),
            ("skill://gamma".to_string(), Uuid::from_u128(3)),
        ];
        assert_eq!(selected_skill_refs(&forward), expected);
        assert_eq!(selected_skill_refs(&reverse), expected);
    }

    #[test]
    fn planning_context_rejects_explicit_disallowed_template() {
        // Pins: an exact published template remains unusable when the session allowlist excludes it.
        let policy = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Allowlist,
            refs: vec!["skill://alpha".to_string()],
            max_visible: None,
        };
        let requested = PinnedExecutionTemplateRef {
            skill_ref: "skill://beta".to_string(),
            revision_uid: Uuid::from_u128(2),
        };
        let error = build_planning_skill_context(
            vec![skill_revision("alpha", 1), skill_revision("beta", 2)],
            Vec::new(),
            &policy,
            Some(&requested),
        )
        .expect_err("disallowed exact template must fail closed");
        assert_eq!(
            error,
            "requested execution template is not an exact permitted pinned published revision"
        );
    }

    #[test]
    fn planning_context_uses_locked_revision_and_rejects_duplicate_exact_revision() {
        // Pins: Allowlist and Pinned keep exact locked behavior, while duplicate
        // exact revisions remain ambiguous and fail closed.
        for mode in [
            AgentSkillPolicyMode::Allowlist,
            AgentSkillPolicyMode::Pinned,
        ] {
            let policy = AgentSkillPolicy {
                mode,
                refs: vec!["skill://alpha".to_string()],
                max_visible: None,
            };
            let locked = build_planning_skill_context(
                vec![skill_revision("alpha", 2)],
                vec![skill_revision("alpha", 1)],
                &policy,
                None,
            )
            .expect("locked policy revision should replace the latest publication");
            assert_selected_skill_revisions(
                &locked,
                &[("skill://alpha".to_string(), Uuid::from_u128(1))],
            );
        }

        let policy = AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Allowlist,
            refs: vec!["skill://alpha".to_string()],
            max_visible: None,
        };
        let duplicate = build_planning_skill_context(
            vec![skill_revision("alpha", 1), skill_revision("alpha", 1)],
            Vec::new(),
            &policy,
            None,
        )
        .expect_err("duplicate exact revisions must fail closed");
        assert_eq!(
            duplicate,
            "duplicate exact skill revision: skill://alpha@00000000-0000-0000-0000-000000000001"
        );

        let duplicate_locked = build_planning_skill_context(
            vec![skill_revision("alpha", 2)],
            vec![skill_revision("alpha", 1), skill_revision("alpha", 1)],
            &policy,
            None,
        )
        .expect_err("duplicate exact locked revisions must fail closed");
        assert_eq!(
            duplicate_locked,
            "duplicate exact locked skill revision: skill://alpha@00000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn accepted_turn_requires_skill_template_provenance_from_planning_snapshot() {
        // Pins: Execution/start cannot admit a fabricated template revision as run provenance.
        let skill_ref = "skill://durable-report"
            .parse::<moa_artifacts::reference::ArtifactRef>()
            .expect("canonical skill ref");
        let pinned_revision_uid = Uuid::from_u128(7);
        let templates = vec![PinnedExecutionTemplate {
            skill_ref: skill_ref.clone(),
            revision_uid: pinned_revision_uid,
            skill_input_schema: json!({"type": "object"}),
            execution_plan: ExecutionPlanTemplate {
                goal: ExecutionGoalTemplate {
                    requirements: Vec::new(),
                    deliverables: Vec::new(),
                    coverage: Vec::new(),
                    constraints: Vec::new(),
                    completion_checks: Vec::new(),
                },
                plan: ExecutionPlanDefinition {
                    schema_version: 1,
                    input_schema: json!({"type": "object"}),
                    output_schema: json!({"type": "object"}),
                    nodes: Vec::new(),
                },
            },
        }];
        let committed_plan_hash = "a".repeat(64);
        let exact = ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref: skill_ref.to_string(),
            skill_template_revision_uid: pinned_revision_uid,
        };
        assert_eq!(
            validate_start_source_provenance(&exact, &committed_plan_hash, &templates),
            Ok(())
        );

        let wrong_revision = ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref: skill_ref.to_string(),
            skill_template_revision_uid: Uuid::from_u128(8),
        };
        assert_eq!(
            validate_start_source_provenance(&wrong_revision, &committed_plan_hash, &templates,),
            Err(ExecutionPlanningContractError::InvalidField {
                field: "skill_template_revision_uid".to_string(),
                message: "must equal one exact template revision in the persisted planning context"
                    .to_string(),
            })
        );

        let noncanonical = ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref: "skill://Durable-Report".to_string(),
            skill_template_revision_uid: pinned_revision_uid,
        };
        assert!(matches!(
            validate_start_source_provenance(
                &noncanonical,
                &committed_plan_hash,
                &templates,
            ),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "skill_template_ref"
        ));
    }

    fn experiment_template_provenance(
        skill_template_ref: String,
        skill_template_revision_uid: Uuid,
    ) -> ExecutionSourceProvenance {
        ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            experiment_run_uid: Uuid::from_u128(21),
            score_run_id: Uuid::from_u128(22),
            trial_uid: Some(Uuid::from_u128(23)),
        }
    }

    fn pinned_execution_template(
        skill_ref: moa_artifacts::reference::ArtifactRef,
        revision_uid: Uuid,
    ) -> PinnedExecutionTemplate {
        PinnedExecutionTemplate {
            skill_ref,
            revision_uid,
            skill_input_schema: json!({"type": "object"}),
            execution_plan: ExecutionPlanTemplate {
                goal: ExecutionGoalTemplate {
                    requirements: Vec::new(),
                    deliverables: Vec::new(),
                    coverage: Vec::new(),
                    constraints: Vec::new(),
                    completion_checks: Vec::new(),
                },
                plan: ExecutionPlanDefinition {
                    schema_version: 1,
                    input_schema: json!({"type": "object"}),
                    output_schema: json!({"type": "object"}),
                    nodes: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn experiment_template_provenance_rejects_unknown_ref_from_planning_snapshot() {
        // Pins: ExperimentTemplate cannot name a canonical skill absent from the immutable context.
        let pinned_ref = "skill://durable-report"
            .parse::<moa_artifacts::reference::ArtifactRef>()
            .expect("canonical pinned skill ref");
        let templates = vec![pinned_execution_template(pinned_ref, Uuid::from_u128(7))];
        let provenance =
            experiment_template_provenance("skill://other-report".to_string(), Uuid::from_u128(7));

        assert_eq!(
            validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
            Err(ExecutionPlanningContractError::InvalidField {
                field: "skill_template_ref".to_string(),
                message:
                    "must equal one canonical template reference in the persisted planning context"
                        .to_string(),
            })
        );
    }

    #[test]
    fn experiment_template_provenance_rejects_wrong_revision_from_planning_snapshot() {
        // Pins: ExperimentTemplate cannot substitute a revision absent from the immutable context.
        let pinned_ref = "skill://durable-report"
            .parse::<moa_artifacts::reference::ArtifactRef>()
            .expect("canonical pinned skill ref");
        let templates = vec![pinned_execution_template(
            pinned_ref.clone(),
            Uuid::from_u128(7),
        )];
        let provenance = experiment_template_provenance(pinned_ref.to_string(), Uuid::from_u128(8));

        assert_eq!(
            validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
            Err(ExecutionPlanningContractError::InvalidField {
                field: "skill_template_revision_uid".to_string(),
                message: "must equal one exact template revision in the persisted planning context"
                    .to_string(),
            })
        );
    }

    #[test]
    fn experiment_template_provenance_rejects_noncanonical_ref() {
        // Pins: ExperimentTemplate stores the byte-identical canonical template reference.
        let pinned_ref = "skill://durable-report"
            .parse::<moa_artifacts::reference::ArtifactRef>()
            .expect("canonical pinned skill ref");
        let templates = vec![pinned_execution_template(pinned_ref, Uuid::from_u128(7))];
        let provenance = experiment_template_provenance(
            "skill://Durable-Report".to_string(),
            Uuid::from_u128(7),
        );

        assert!(matches!(
            validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "skill_template_ref"
        ));
    }

    #[test]
    fn experiment_template_provenance_accepts_exact_persisted_revision() {
        // Pins: ExperimentTemplate admits the exact canonical ref and revision pinned in context.
        let pinned_ref = "skill://durable-report"
            .parse::<moa_artifacts::reference::ArtifactRef>()
            .expect("canonical pinned skill ref");
        let pinned_revision_uid = Uuid::from_u128(7);
        let templates = vec![pinned_execution_template(
            pinned_ref.clone(),
            pinned_revision_uid,
        )];
        let provenance =
            experiment_template_provenance(pinned_ref.to_string(), pinned_revision_uid);

        assert_eq!(
            validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
            Ok(())
        );
    }

    #[test]
    fn capability_catalog_uses_live_execution_metadata_and_omits_non_invocable_declarations() {
        // Pins: only router-owned tools and artifact wrappers with live backing tools enter the catalog.
        let mut registry = ToolRegistry::default_local();
        registry
            .register_mcp_tool(
                "github",
                McpDiscoveredTool {
                    name: "github_issue_create".to_string(),
                    description: "create an issue".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {"title": {"type": "string"}},
                        "required": ["title"]
                    }),
                },
            )
            .expect("MCP fixture should register");
        let revisions = vec![
            revision(
                "publish-note",
                10,
                document(
                    "publish-note",
                    ArtifactKind::Action,
                    serde_json::from_value(json!({
                        "type": "action",
                        "spec": {
                            "id": "publish-note",
                            "description": "publish a note",
                            "tool_name": "bash",
                            "input_schema": {"type": "object", "required": ["body"]},
                            "output_schema": {"type": "object", "required": ["published"]},
                            "admin_review_required": true,
                            "ui": {}
                        }
                    }))
                    .expect("action definition fixture should decode"),
                ),
            ),
            revision(
                "missing-action",
                11,
                document(
                    "missing-action",
                    ArtifactKind::Action,
                    serde_json::from_value(json!({
                        "type": "action",
                        "spec": {
                            "id": "missing-action",
                            "description": "not executable",
                            "tool_name": "not_registered",
                            "input_schema": {},
                            "output_schema": {},
                            "ui": {}
                        }
                    }))
                    .expect("action definition fixture should decode"),
                ),
            ),
            revision(
                "code-skill",
                12,
                document(
                    "code-skill",
                    ArtifactKind::Skill,
                    serde_json::from_value(json!({
                        "type": "skill",
                        "spec": {
                            "instructions": {"path": "SKILL.md"},
                            "inputs": {},
                            "outputs": {},
                            "actions": [{
                                "id": "run-code",
                                "description": "run unowned code",
                                "kind": "code",
                                "runtime": "python",
                                "entrypoint": "main.py",
                                "input_schema": {},
                                "output_schema": {},
                                "ui": {}
                            }],
                            "connectors": [],
                            "allowed_tools": [],
                            "ui": {}
                        }
                    }))
                    .expect("skill definition fixture should decode"),
                ),
            ),
        ];

        let response = build_capability_response(
            &registry.capability_registrations(),
            &revisions,
            &["connection-123".to_string()],
        )
        .expect("capability catalog should build");

        let action = response
            .catalog
            .capabilities
            .iter()
            .find(|entry| entry.reference.name == "action://publish-note")
            .expect("resolved action should be catalogued");
        assert_eq!(action.reference.version, Uuid::from_u128(10).to_string());
        assert_eq!(
            action.input_schema,
            json!({"type": "object", "required": ["body"]})
        );
        assert_eq!(
            action.output_schema,
            json!({"type": "object", "required": ["published"]})
        );
        assert_eq!(
            action.default_effect,
            moa_core::types::action_policy::ActionPolicyEffect::AdminReview
        );
        assert!(matches!(
            &action.source,
            CapabilitySource::ActionArtifact { tool_name, revision_uid, .. }
                if tool_name == "bash" && *revision_uid == Uuid::from_u128(10)
        ));
        assert_eq!(action.estimate.tool_calls, 1);
        assert_eq!(action.estimate.tasks, 1);

        let mcp = response
            .catalog
            .capabilities
            .iter()
            .find(|entry| entry.reference.name == "github_issue_create")
            .expect("connected MCP tool should be catalogued");
        assert_eq!(mcp.execution_class, ExecutionClass::External);
        assert_eq!(
            mcp.input_schema,
            json!({
                "type": "object",
                "properties": {"title": {"type": "string"}},
                "required": ["title"]
            })
        );
        assert!(matches!(
            &mcp.source,
            CapabilitySource::McpTool { server, name }
                if server == "github" && name == "github_issue_create"
        ));
        assert!(!mcp.reference.version.is_empty());

        assert!(!response.catalog.capabilities.iter().any(|entry| {
            matches!(
                &entry.source,
                CapabilitySource::SkillCode { .. } | CapabilitySource::Knowledge { .. }
            ) || entry.reference.name == "action://missing-action"
                || entry.reference.name == "connection-123"
        }));
        assert_eq!(
            response
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.code)
                .collect::<Vec<_>>(),
            vec![
                CapabilityCatalogDiagnosticCode::UnresolvedActionTool,
                CapabilityCatalogDiagnosticCode::ConnectionOnlyDataSource,
                CapabilityCatalogDiagnosticCode::UnownedSkillCode,
            ]
        );
    }

    #[test]
    fn skill_regression_authority_uses_governed_catalog_and_exact_skill_allowlist() {
        // Pins: review compilation derives capability and skill authority from the same
        // production catalog builder, including the exact draft without duplicating its
        // stable skill ref when a previous revision is published.
        let registry = ToolRegistry::default_local();
        let published = skill_revision("reviewed-skill", 20);
        let mut draft = skill_revision("reviewed-skill", 21);
        draft.status = ArtifactStatus::Draft;
        draft.published_at = None;
        let authority = build_skill_regression_compile_authority(
            &registry.capability_registrations(),
            &[published, draft, skill_revision("dependency", 22)],
            &[],
        )
        .expect("skill regression authority should resolve");

        assert!(
            authority
                .catalog
                .capabilities
                .iter()
                .any(|capability| capability.reference.name == "file_read")
        );
        assert_eq!(
            authority.authorization.capability_refs,
            authority
                .catalog
                .capabilities
                .iter()
                .map(|capability| capability.reference.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            authority
                .authorization
                .skill_refs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![
                "skill://dependency".to_string(),
                "skill://reviewed-skill".to_string(),
            ]
        );
    }

    #[test]
    fn execution_external_wait_payload_is_validated_against_node_schema() {
        // Pins: review and signal handlers cannot persist caller-supplied output
        // that the active immutable plan would reject.
        let plan = serde_json::from_value(json!({
            "schema_version": 1,
            "input_schema": {},
            "output_schema": {},
            "nodes": [{
                "id": "review",
                "requirement_ids": ["approval"],
                "depends_on": [],
                "when": null,
                "input": {},
                "output_schema": {
                    "type": "object",
                    "required": ["approved"],
                    "properties": {"approved": {"type": "boolean"}}
                },
                "operation": {"kind": "review", "prompt": "Approve?"},
                "retry": {"max_attempts": 1, "initial_backoff_ms": 1, "max_backoff_ms": 1},
                "budget": null
            }]
        }))
        .expect("plan fixture should decode");

        validate_external_wait_payload(&plan, "review", &json!({"approved": true}))
            .expect("valid external output should pass");
        assert!(
            validate_external_wait_payload(&plan, "review", &Value::String("bypass".to_string()))
                .is_err(),
            "schema-invalid caller output must be rejected"
        );
    }

    #[test]
    fn replan_failure_counts_include_append_only_superseded_history() {
        // Pins: superseding a NeedsReplan task cannot erase its normalized
        // failure occurrence from the next amendment stop evaluation.
        let failure = FailureFingerprintInput {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
            node_id: "collect".to_string(),
            capability_ref: None,
            message: " Source   Unavailable ".to_string(),
        };
        let fingerprint = failure_fingerprint(&failure).expect("failure should hash");
        let history = vec![
            json!({
                "failure_fingerprint": fingerprint,
                "failure_fingerprint_count": 1
            }),
            json!({
                "failure_fingerprint": fingerprint,
                "failure_fingerprint_count": 2
            }),
        ];
        assert_eq!(
            durable_failure_fingerprint_counts(&history),
            [(fingerprint, 2)].into_iter().collect()
        );
    }

    #[test]
    fn replan_history_detects_duplicate_operations_without_exact_replay() {
        // Pins: the service derives semantic loop identity from persisted amendment values, so a
        // later base revision and changed prose reach DuplicateAmendment without colliding with
        // the repository's full amendment replay hash.
        let operation = PlanAmendmentOperation::AddNode {
            node: ExecutionNode {
                id: "replacement".to_string(),
                requirement_ids: vec!["req_report".to_string()],
                depends_on: vec!["prepared".to_string()],
                when: None,
                input: json!({}),
                output_schema: json!({"type": "object"}),
                operation: ExecutionOperation::Output {
                    value: json!({"report": true}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            },
        };
        let first = PlanAmendment {
            schema_version: 1,
            base_plan_revision: 1,
            reason: "first explanation".to_string(),
            evidence: json!({"source": "first"}),
            operations: vec![operation.clone()],
        };
        let proposed = PlanAmendment {
            schema_version: 1,
            base_plan_revision: 2,
            reason: "different explanation".to_string(),
            evidence: json!({"source": "second"}),
            operations: vec![operation],
        };
        assert_ne!(
            amendment_hash(&first).expect("hash first exact amendment"),
            amendment_hash(&proposed).expect("hash proposed exact amendment")
        );
        let seen = durable_amendment_operation_fingerprints(&[json!({"amendment": first})])
            .expect("persisted amendment values should fingerprint");
        let decision = evaluate_replan_loop_stop(ReplanLoopEvaluationRequest {
            proposed_amendment_fingerprint: amendment_operations_fingerprint(&proposed)
                .expect("fingerprint proposed operations"),
            seen_amendment_fingerprints: seen,
            failure_fingerprint_counts: BTreeMap::new(),
            current_failure: None,
            unresolved_requirement_ids: BTreeSet::from(["req_report".to_string()]),
            amendment: proposed,
            config: moa_core::config::ExecutionConfig::default(),
        });
        assert_eq!(
            decision,
            ReplanDecision::Stop {
                reason: ReplanStopReason::DuplicateAmendment
            }
        );
    }

    #[test]
    fn remove_only_amendment_reaches_no_progress_before_validation_rejection() {
        // Pins: the service can classify structurally invalid remove-only proposals through the
        // shared pure loop policy instead of exposing a compiler-validation error.
        let amendment = PlanAmendment {
            schema_version: 1,
            base_plan_revision: 4,
            reason: "remove failed work".to_string(),
            evidence: json!({}),
            operations: vec![PlanAmendmentOperation::RemovePendingNode {
                node_id: "failed".to_string(),
            }],
        };
        assert_eq!(
            evaluate_replan_loop_stop(ReplanLoopEvaluationRequest {
                proposed_amendment_fingerprint: amendment_operations_fingerprint(&amendment)
                    .expect("fingerprint remove-only operations"),
                seen_amendment_fingerprints: BTreeSet::new(),
                failure_fingerprint_counts: BTreeMap::new(),
                current_failure: None,
                unresolved_requirement_ids: BTreeSet::from(["req_report".to_string()]),
                amendment,
                config: moa_core::config::ExecutionConfig::default(),
            }),
            ReplanDecision::Stop {
                reason: ReplanStopReason::NoProgress
            }
        );
    }

    #[test]
    fn exact_terminalized_input_replay_recovers_audience_from_append_only_audit() {
        // Pins: replacing NeedsInput with a typed terminal admission failure
        // does not make the exact old-generation delivery fail audience checks.
        let needs_input = moa_artifacts::execution_plan::ExecutionTaskOutcome {
            schema_version: 1,
            usage: moa_artifacts::execution_plan::ExecutionUsage {
                cost_microusd: 0,
                tokens: 0,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
            result: moa_artifacts::execution_plan::ExecutionTaskResult::NeedsInput {
                question: "continue?".to_string(),
                audience: moa_artifacts::execution_plan::InputAudience::User,
            },
        };
        let terminal = moa_artifacts::execution_plan::ExecutionTaskOutcome {
            schema_version: 1,
            usage: needs_input.usage.clone(),
            result: moa_artifacts::execution_plan::ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
                message: "deadline elapsed".to_string(),
            },
        };
        let audit = vec![json!({
            "received_generation": 1,
            "accepted": true,
            "outcome": needs_input
        })];
        assert_eq!(
            persisted_input_audience(2, Some(&terminal), &audit, 1),
            Some(moa_artifacts::execution_plan::InputAudience::User)
        );
        assert_eq!(
            persisted_input_audience(2, Some(&terminal), &audit, 0),
            None
        );
    }
}
