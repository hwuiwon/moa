//! Restate service for compiler-ready execution capabilities.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, ExecutionBudgetLimit, ExecutionPlanDefinition,
};
use moa_artifacts::execution_plan::{
    ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage,
    PlanAmendment, PlanAmendmentOperation,
};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::release::TenantScope;
use moa_artifacts::skill::{SkillActionDefinition, SkillActionKind};
use moa_authz_schema::Relation;
use moa_config::ExecutionConfig;
use moa_core::types::memory::RlsContext;
use moa_core::types::tools::{
    ToolDefinition, ToolPolicySpec, ToolRollbackDefinition, ToolRollbackValueSource,
};
use moa_core::{
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
use moa_execution::capability::{
    CapabilitiesListRequest, CapabilitiesListResponse, CapabilityCatalogDiagnostic,
    CapabilityCatalogDiagnosticCode, CapabilityPolicyContext, CapabilityRollbackContract,
    CapabilitySource, ExecutionCapability, ExecutionCapabilityCatalog, ExecutionClass,
    ExecutionEstimate, ExecutionHash, amendment_hash, amendment_operations_fingerprint,
    capability_version, plan_hash,
};
use moa_execution::{
    budget::{BudgetLedger, estimate_fits_limit},
    compiler::{CompileExecutionRequest, ValidateAmendmentRequest, compile, validate_amendment},
    completion::{
        CompletionEvaluationRequest, cancellation_terminal_evidence, evaluate_completion,
        execution_terminal_reason, terminal_evidence_from_evaluation,
        terminal_projection_from_evaluation,
    },
    replan::{
        ReplanDecision, ReplanEvaluationRequest, ReplanLoopEvaluationRequest,
        evaluate_replan_loop_stop, evaluate_replan_resource_stop, evaluate_replan_stop,
        failure_fingerprint, replan_stop_gaps, replan_stop_status,
    },
    repository::{
        AmendmentReplayOutcome, AmendmentWrite, ConfirmationConflict, ConfirmationOutcome,
        ExecutionRepository, ExecutionRunPageRequest, ExecutionRunRecord, ExecutionScope,
        ExecutionTaskPageRequest, ExecutionTaskRecord, NewExecutionPlanningContext,
        NewExecutionRun, PlanningContextWriteOutcome, ReplanStopReceipt, TaskOutcomeWrite,
        TerminalFenceOutcome, TransitionOutcome, TransitionRejection, ValidatedAmendment,
    },
    schema::validate_instance,
    state::{
        ExecutionRunStatus, ExecutionTaskProjection, ExecutionTaskStatus, ExecutionTerminalCause,
        ExecutionTerminalReason, FailureFingerprintInput, PendingExecutionTerminal,
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
use moa_hands::ToolExecution;
use moa_knowledge::repository::{
    PostgresKnowledgeRepository, connection::KnowledgeConnectionRepository,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_security::stricter_effect;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::connector_catalog::ScopedConnectorCatalogProvider;
use crate::handlers::authz_shim::AuthzEnforcer;
use crate::objects::session::{ExecutionRunStartedDelivery, SessionClient};
use crate::restate_identity::with_identity_headers;
use crate::services::llm_gateway::{LLMCompletionOwner, cancel_completion_owner_from_service};
use crate::workflows::errors::moa_error_to_status_handler_error;
use crate::workflows::execution_run::ExecutionRunClient;
use crate::workflows::execution_task::ExecutionTaskClient;

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
    connector_catalog: ScopedConnectorCatalogProvider,
    config: ExecutionConfig,
    session_store: Arc<PostgresSessionStore>,
    authz: AuthzEnforcer,
}

impl ExecutionImpl {
    /// Creates the execution service with its live invocation registry.
    #[must_use]
    pub(crate) fn new(
        pool: sqlx::PgPool,
        connector_catalog: ScopedConnectorCatalogProvider,
        config: ExecutionConfig,
        session_store: Arc<PostgresSessionStore>,
        authz: AuthzEnforcer,
    ) -> Self {
        Self {
            pool,
            connector_catalog,
            config,
            session_store,
            authz,
        }
    }
}

#[cfg(feature = "integration")]
pub mod capability_catalog;
#[cfg(not(feature = "integration"))]
pub(crate) mod capability_catalog;
pub(crate) mod handlers;
mod planning_context;
mod start;
mod support;

#[cfg(test)]
mod tests;
