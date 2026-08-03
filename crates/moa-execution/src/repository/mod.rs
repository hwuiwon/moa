//! Scoped PostgreSQL persistence for durable execution runs and logical tasks.

mod audit;
mod audit_codec;
mod materialize;
mod outcome;
mod outcome_support;
mod projection;
mod rows;
mod run;
mod sql;
mod task;
mod terminal;
mod transition;

use std::{collections::BTreeMap, str::FromStr};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionCitation, ExecutionGoalContract, ExecutionOperation,
    ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage, PlanAmendment,
};
use moa_core::{
    types::contact::ContactId,
    types::execution_planning::{
        ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlannerCallKind,
        ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload,
        ExecutionRouteClassifierOutcome, ExecutionRouteKind, ExecutionRouteProvenance,
        ExecutionRouteSource, ExecutionRouteStage, ExecutionRouteUsage, ExecutionSourceProvenance,
        ExecutionStrategy, route_provenance_semantically_equal, validate_planning_audit_envelope,
    },
    types::identifiers::{SessionId, TenantId, UserId},
};
use moa_db::ScopedConn;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;

use crate::{
    Error, Result,
    budget::{BudgetLedger, BudgetReconciliation},
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash, amendment_hash,
    },
    compiler::CanonicalExecutionPlan,
    completion::{
        CompletionEvaluation, cancellation_terminal_evidence, execution_terminal_reason,
        run_status_from_completion, terminal_evidence_from_evaluation,
    },
    replan::failure_fingerprint,
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionRunStatus, ExecutionSourceKind,
        ExecutionTaskId, ExecutionTaskProjection, ExecutionTaskStatus, ExecutionTerminalCause,
        ExecutionTerminalEvidence, ExecutionTerminalReason, FailureFingerprintInput, LogicalTask,
        LogicalTaskKind, TerminalProjection, WaitingReason, cancelled_task_outcome,
        run_status_after_task_outcome, run_status_from_terminal_projection,
        task_outcome_is_terminal, task_status_from_outcome,
    },
    wire::{
        ExecutionActionReviewResolution, ExecutionPlanningContextSnapshot,
        ExecutionTemplateAdmissionRequest, ExecutionTerminalDelivery, PinnedInstructionSkill,
        execution_terminal_delivery_from_state,
    },
};

const DEFAULT_RUN_PAGE_LIMIT: u32 = 100;
const MAX_RUN_PAGE_LIMIT: u32 = 1_000;
const DEFAULT_TASK_PAGE_LIMIT: u32 = 100;
const MAX_TASK_PAGE_LIMIT: u32 = 1_000;
const EXECUTION_AUDIT_NAMESPACE: Uuid = Uuid::from_u128(0x7b83_c5c2_5cf7_5fa0_8eb6_2d7c_6e0f_1d11);

/// Database scope used for one repository operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionScope {
    /// Explicit platform control-plane access. New rows still declare their tenant/contact owner.
    ControlPlane,
    /// Tenant-owned rows whose `contact_id` is null.
    Tenant {
        /// Owning tenant.
        tenant_id: TenantId,
    },
    /// Rows owned by one contact inside a tenant.
    Contact {
        /// Owning tenant.
        tenant_id: TenantId,
        /// Owning contact.
        contact_id: ContactId,
    },
}

impl ExecutionScope {
    async fn begin<'p>(self, pool: &'p PgPool) -> Result<ScopedConn<'p>> {
        if matches!(
            self,
            Self::Contact { contact_id, .. } if contact_id.0.is_nil()
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "execution repository contact scope must not use a nil contact"
                    .to_string(),
            });
        }
        let mut conn = match self {
            Self::ControlPlane => ScopedConn::begin_control_plane(pool).await,
            Self::Tenant { tenant_id } => ScopedConn::begin_tenant(pool, tenant_id).await,
            Self::Contact {
                tenant_id,
                contact_id,
            } => ScopedConn::begin_contact(pool, tenant_id, contact_id).await,
        }
        .map_err(storage_error)?;
        conn.assume_app_role().await.map_err(storage_error)?;
        Ok(conn)
    }

    fn permits_owner(self, tenant_id: TenantId, contact_id: Option<ContactId>) -> bool {
        match self {
            Self::ControlPlane => true,
            Self::Tenant {
                tenant_id: scoped_tenant,
            } => scoped_tenant == tenant_id && contact_id.is_none(),
            Self::Contact {
                tenant_id: scoped_tenant,
                contact_id: scoped_contact,
            } => scoped_tenant == tenant_id && contact_id == Some(scoped_contact),
        }
    }
}

async fn install_execution_scope(
    tx: &mut Transaction<'_, Postgres>,
    scope: ExecutionScope,
) -> Result<()> {
    let (tenant_id, storage_partition_id, contact_id, control_plane) = match scope {
        ExecutionScope::ControlPlane => (None, None, None, true),
        ExecutionScope::Tenant { tenant_id } => (
            Some(tenant_id.to_string()),
            Some(
                moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string(),
            ),
            Some(String::new()),
            false,
        ),
        ExecutionScope::Contact {
            tenant_id,
            contact_id,
        } => (
            Some(tenant_id.to_string()),
            Some(
                moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string(),
            ),
            Some(contact_id.to_string()),
            false,
        ),
    };
    sqlx::query(
        r#"
        SELECT
            pg_catalog.set_config('moa.tenant_id', $1, true),
            pg_catalog.set_config('moa.storage_partition_id', $2, true),
            pg_catalog.set_config('moa.contact_id', $3, true),
            pg_catalog.set_config('moa.control_plane', $4, true)
        "#,
    )
    .bind(tenant_id.as_deref().unwrap_or(""))
    .bind(storage_partition_id.as_deref().unwrap_or(""))
    .bind(contact_id.as_deref().unwrap_or(""))
    .bind(if control_plane { "true" } else { "false" })
    .execute(&mut **tx)
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

/// Input used to create one immutable execution run snapshot.
#[derive(Clone, Debug)]
pub struct NewExecutionRun {
    /// Tenant that owns the run.
    pub tenant_id: TenantId,
    /// Optional contact owner; null denotes tenant-owned work.
    pub contact_id: Option<ContactId>,
    /// Parent session that requested the run.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that originated the run.
    pub originating_user_sequence_num: u64,
    /// Immutable planning-context row used for admission.
    pub planning_context_uid: Uuid,
    /// Expected canonical planning-context hash.
    pub planning_context_hash: ExecutionHash,
    /// Authenticated tenant user that owns the run.
    pub owner_user_id: UserId,
    /// Immutable user-derived goal contract.
    pub goal: ExecutionGoalContract,
    /// Initial canonical plan, also installed as revision one.
    pub plan: CanonicalExecutionPlan,
    /// Exact immutable capability catalog used by compilation.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact immutable capability and skill authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Sorted exact instruction-skill revisions available to task-local agents.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Skill-template or generated-plan provenance.
    pub source_provenance: ExecutionSourceProvenance,
    /// Structured run input.
    pub input: Value,
    /// Initial status, which must be queued or awaiting confirmation.
    pub status: ExecutionRunStatus,
    /// Approved resource envelope displayed or accepted for this run.
    pub approved_budget: ExecutionBudgetLimit,
    /// Optional caller idempotency key scoped to tenant and contact.
    pub idempotency_key: Option<String>,
}

/// Permanent replay row for one external execution-template admission operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTemplateAdmissionRecord {
    /// Stable operation identity reserved before any Session mutation.
    pub operation_uid: Uuid,
    /// Canonical fingerprint of the complete first request.
    pub request_fingerprint: String,
    /// Exact persisted objective sequence, when committed.
    pub originating_user_sequence_num: Option<u64>,
    /// Exact execution run UID, when committed.
    pub execution_run_uid: Option<Uuid>,
}

/// Persisted execution-run projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionRunRecord {
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Originating parent session.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that originated the run.
    pub originating_user_sequence_num: u64,
    /// Immutable planning-context row used for admission.
    pub planning_context_uid: Uuid,
    /// Canonical planning-context hash used for admission.
    pub planning_context_hash: ExecutionHash,
    /// Originating tenant user.
    pub owner_user_id: UserId,
    /// Immutable goal contract.
    pub goal: ExecutionGoalContract,
    /// Immutable initial canonical plan.
    pub initial_plan: CanonicalExecutionPlan,
    /// Current canonical plan.
    pub active_plan: CanonicalExecutionPlan,
    /// Immutable initial plan hash.
    pub initial_plan_hash: ExecutionHash,
    /// Current active plan hash.
    pub active_plan_hash: ExecutionHash,
    /// Exact plan hash accepted by confirmation, when confirmation occurred.
    pub confirmed_plan_hash: Option<ExecutionHash>,
    /// Current one-based plan revision.
    pub plan_revision: u64,
    /// Append-only amendment metadata.
    pub plan_history: Vec<Value>,
    /// Exact immutable persisted capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact immutable persisted authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Sorted exact persisted instruction-skill revisions.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Skill-template or generated-plan provenance.
    pub source_provenance: ExecutionSourceProvenance,
    /// Normalized source cohort persisted by the execution-analytics schema.
    pub source_kind: ExecutionSourceKind,
    /// Exact pinned skill-template reference for template-backed runs.
    pub skill_template_ref: Option<String>,
    /// Exact pinned skill-template revision for template-backed runs.
    pub skill_template_revision_uid: Option<Uuid>,
    /// Structured run input.
    pub input: Value,
    /// Terminal structured output, when present.
    pub output: Option<Value>,
    /// Persisted completion-check evidence.
    pub completion_check_results: Vec<Value>,
    /// Explicit terminal completion gaps.
    pub terminal_gaps: Vec<String>,
    /// Immutable typed terminal cause and requirement counts, when terminal.
    pub terminal_evidence: Option<ExecutionTerminalEvidence>,
    /// Exact normalized terminal reason, present exactly for terminal runs.
    pub terminal_reason: Option<ExecutionTerminalReason>,
    /// Current durable run status.
    pub status: ExecutionRunStatus,
    /// Approved resource limits.
    pub approved_budget: ExecutionBudgetLimit,
    /// Resources held by nonterminal tasks.
    pub reserved: ExecutionEstimate,
    /// Reconciled actual resources and terminal logical tasks.
    pub consumed: ExecutionEstimate,
    /// Whether actual usage exceeded a reservation or approved limit.
    pub budget_overrun: bool,
    /// Number of materialized tasks.
    pub progress_total_tasks: u64,
    /// Number of successfully completed tasks.
    pub progress_completed_tasks: u64,
    /// Number of failed tasks.
    pub progress_failed_tasks: u64,
    /// Number of cancelled tasks.
    pub progress_cancelled_tasks: u64,
    /// Exact current scheduler wait reasons.
    pub waiting_reasons: Vec<WaitingReason>,
    /// Monotonic epoch incremented by scheduling-relevant mutations.
    pub wake_epoch: u64,
    /// Last scheduler epoch acknowledged by compare-and-set.
    pub processed_wake_epoch: u64,
    /// Scoped idempotency key.
    pub idempotency_key: Option<String>,
    /// Cancellation reason, when cancelled.
    pub cancellation_reason: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Database-owned timestamp when the run first became queued.
    pub queued_at: Option<DateTime<Utc>>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// First task-start timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Terminal timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Confirmation timestamp.
    pub confirmed_at: Option<DateTime<Utc>>,
}

/// Input used to insert one immutable origin-bound planning-context snapshot.
#[derive(Clone, Debug)]
pub struct NewExecutionPlanningContext {
    /// Exact immutable snapshot whose canonical bytes are hashed.
    pub snapshot: ExecutionPlanningContextSnapshot,
    /// Domain-separated hash of the canonical snapshot bytes.
    pub planning_context_hash: ExecutionHash,
}

/// Persisted immutable planning-context projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionPlanningContextRecord {
    /// Durable planning-context identifier.
    pub planning_context_uid: Uuid,
    /// Exact immutable snapshot.
    pub snapshot: ExecutionPlanningContextSnapshot,
    /// Domain-separated hash of the canonical snapshot bytes.
    pub planning_context_hash: ExecutionHash,
    /// Database-owned creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Result of inserting or replaying one unique origin-bound planning context.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanningContextWriteOutcome {
    /// The immutable snapshot was inserted.
    Created(ExecutionPlanningContextRecord),
    /// The exact immutable snapshot already existed for the origin.
    Replayed(ExecutionPlanningContextRecord),
    /// The unique origin already exists with different immutable bytes or scope.
    Conflict,
}

/// Persisted low-cardinality evidence for one route-audit insertion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteAuditEvidence {
    /// Deterministic UUIDv5 audit identifier.
    pub audit_uid: Uuid,
    /// Respond, Execute, or NeedsInput decision.
    pub decision: ExecutionRouteKind,
    /// Selected strategy, present exactly for Execute.
    pub strategy: Option<ExecutionStrategy>,
    /// Redacted trusted-bypass or classifier provenance.
    pub provenance: ExecutionRouteProvenance,
    /// First durable acceptance timestamp.
    pub accepted_at: DateTime<Utc>,
}

/// Durable result of inserting one normalized route audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RouteAuditWriteOutcome {
    /// This transaction inserted the first route row.
    Applied(RouteAuditEvidence),
    /// The exact semantic route row already existed.
    Replayed(RouteAuditEvidence),
    /// The logical key already carries different route semantics.
    Conflict {
        /// Deterministic audit identifier for the conflicting logical key.
        audit_uid: Uuid,
    },
}

/// Persisted low-cardinality evidence for one planner-call audit insertion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannerCallAuditEvidence {
    /// Deterministic UUIDv5 audit identifier.
    pub audit_uid: Uuid,
    /// Exact closed planner call kind.
    pub call: ExecutionPlannerCallKind,
    /// Exact closed planner outcome.
    pub outcome: ExecutionPlannerOutcome,
    /// First persisted measured duration.
    pub duration_micros: u64,
    /// Candidate hash when required by the outcome.
    pub candidate_hash: Option<String>,
}

/// Durable result of inserting one normalized planner-call audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PlannerCallAuditWriteOutcome {
    /// This transaction inserted the first planner-call row.
    Applied(PlannerCallAuditEvidence),
    /// The exact semantic planner-call row already existed.
    Replayed(PlannerCallAuditEvidence),
    /// The logical key already carries different planner-call semantics.
    Conflict {
        /// Deterministic audit identifier for the conflicting logical key.
        audit_uid: Uuid,
    },
}

/// Persisted low-cardinality evidence for one compiler-audit insertion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompileAuditEvidence {
    /// Deterministic UUIDv5 audit identifier.
    pub audit_uid: Uuid,
    /// Exact closed compiler source.
    pub source: ExecutionCompileSource,
    /// Exact closed compiler outcome.
    pub outcome: ExecutionCompileOutcome,
    /// First persisted measured duration.
    pub duration_micros: u64,
    /// Hash of the strict compile candidate.
    pub candidate_hash: String,
    /// Accepted final plan hash, when compilation succeeded.
    pub final_plan_hash: Option<String>,
}

/// Durable result of inserting one normalized compiler audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CompileAuditWriteOutcome {
    /// This transaction inserted the first compiler row.
    Applied(CompileAuditEvidence),
    /// The exact semantic compiler row already existed.
    Replayed(CompileAuditEvidence),
    /// The logical key already carries different compiler semantics.
    Conflict {
        /// Deterministic audit identifier for the conflicting logical key.
        audit_uid: Uuid,
    },
}

/// Persisted logical-task projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionTaskRecord {
    /// Stable logical task identifier.
    pub task_id: ExecutionTaskId,
    /// Owning run.
    pub run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Stable plan node ID.
    pub node_id: String,
    /// Stable ordinary, map, reducer, or verifier item key.
    pub item_key: String,
    /// Goal requirements served by this task.
    pub requirement_ids: Vec<String>,
    /// Plan revision that created this task.
    pub plan_revision: u64,
    /// Current durable task status.
    pub status: ExecutionTaskStatus,
    /// One-based execution attempt.
    pub attempt: u32,
    /// One-based dispatch generation fence.
    pub generation: u64,
    /// Resolved structured task input.
    pub input: Value,
    /// Append-only ordered payloads supplied by input resumes.
    pub resume_input_history: Vec<Value>,
    /// Executable task descriptor.
    pub kind: LogicalTaskKind,
    /// Retry policy serialized with the task.
    pub retry: moa_artifacts::execution_plan::RetryPolicy,
    /// Worst-case reservation requested by this task.
    pub estimate: ExecutionEstimate,
    /// Remaining reservation currently held by the task.
    pub reserved: ExecutionEstimate,
    /// Cumulative actual resource usage.
    pub actual: ExecutionUsage,
    /// Whether this logical task has been reconciled terminally.
    pub actual_tasks: u64,
    /// Latest accepted outcome.
    pub current_outcome: Option<ExecutionTaskOutcome>,
    /// Current structured output.
    pub output: Option<Value>,
    /// Current structured error details.
    pub error: Option<Value>,
    /// Current accepted citations.
    pub citations: Vec<ExecutionCitation>,
    /// Immutable attempt/generation dispatch history.
    pub generation_history: Vec<Value>,
    /// Append-only accepted and rejected outcome audit.
    pub outcome_audit: Vec<Value>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// Reservation timestamp.
    pub reserved_at: Option<DateTime<Utc>>,
    /// First running timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Terminal timestamp.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Result of confirming an awaiting execution run.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfirmationOutcome {
    /// The displayed plan hash matched and the approved budget was persisted.
    Confirmed(ExecutionRunRecord),
    /// The same plan hash and budget were already confirmed.
    AlreadyConfirmed(ExecutionRunRecord),
    /// No visible run exists.
    NotFound,
    /// Confirmation differed from persisted state and changed nothing.
    Conflict(ConfirmationConflict),
}

/// Stable reason a run confirmation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationConflict {
    /// The displayed plan hash no longer matches the active plan.
    PlanHashMismatch,
    /// A replay supplied a different approved budget.
    BudgetMismatch,
    /// The run is not awaiting confirmation and is not an exact confirmed replay.
    InvalidStatus,
}

/// Closed first-materialization marker for one aggregate execution node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionNodeMaterialization {
    /// One map node with its exact resolved item count.
    Map {
        /// Stable plan node identifier.
        node_id: String,
        /// Exact fan-out item count, including zero.
        fanout_items: u64,
    },
    /// One reducer node with its exact tree depth.
    Reduce {
        /// Stable plan node identifier.
        node_id: String,
        /// Exact reducer tree depth.
        reducer_depth: u64,
    },
}

impl ExecutionNodeMaterialization {
    fn node_id(&self) -> &str {
        match self {
            Self::Map { node_id, .. } | Self::Reduce { node_id, .. } => node_id,
        }
    }

    const fn kind_label(&self) -> &'static str {
        match self {
            Self::Map { .. } => "map",
            Self::Reduce { .. } => "reduce",
        }
    }

    const fn fanout_items(&self) -> Option<u64> {
        match self {
            Self::Map { fanout_items, .. } => Some(*fanout_items),
            Self::Reduce { .. } => None,
        }
    }

    const fn reducer_depth(&self) -> Option<u64> {
        match self {
            Self::Map { .. } => None,
            Self::Reduce { reducer_depth, .. } => Some(*reducer_depth),
        }
    }
}

/// First-applied task and aggregate-marker evidence from one materialization transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializationEvidence {
    /// Complete persisted task projections for the requested node.
    pub tasks: Vec<ExecutionTaskRecord>,
    /// Stable task IDs inserted by this transaction, sorted ascending.
    pub inserted_task_ids: Vec<ExecutionTaskId>,
    /// First-applied aggregate marker, when this was a map or reducer node.
    pub marker: Option<ExecutionNodeMaterialization>,
}

/// Durable outcome of one node materialization transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum MaterializationOutcome {
    /// This transaction first applied task rows or its aggregate marker.
    Applied(MaterializationEvidence),
    /// The exact tasks and marker were already committed.
    Replayed {
        /// Complete persisted task projections for the requested node.
        tasks: Vec<ExecutionTaskRecord>,
    },
    /// The durable node identity already carries different materialization semantics.
    Conflict,
}

/// Result of atomically reserving one task's five-dimensional estimate.
#[derive(Clone, Debug, PartialEq)]
pub enum ReservationOutcome {
    /// Budget was reserved and the task moved to reserved.
    Reserved(ExecutionTaskRecord),
    /// The same generation is already reserved.
    AlreadyReserved(ExecutionTaskRecord),
    /// Admission failed and the typed terminal outcome committed atomically.
    Terminalized(Box<ReservationTerminalization>),
    /// The same generation's terminal admission result was already committed.
    AlreadyTerminalized(Box<ReservationTerminalization>),
    /// No visible task exists.
    NotFound,
    /// Reservation changed nothing.
    Rejected(ReservationRejection),
}

/// Stable reason a task reservation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationRejection {
    /// The supplied generation is stale or otherwise mismatched.
    GenerationMismatch,
    /// The task is not pending.
    InvalidTaskStatus,
    /// The run is not queued or running.
    InvalidRunStatus,
    /// The approved deadline has elapsed.
    DeadlineElapsed,
    /// At least one resource dimension cannot be reserved.
    BudgetExceeded,
}

/// Committed task and run projections for a terminal reservation rejection.
#[derive(Clone, Debug, PartialEq)]
pub struct ReservationTerminalization {
    /// Updated run with reconciled accounting and advanced wake epoch.
    pub run: ExecutionRunRecord,
    /// Generation-fenced typed terminal task projection.
    pub task: ExecutionTaskRecord,
    /// Stable admission reason represented by the task outcome.
    pub rejection: ReservationRejection,
}

/// Result of a generation-fenced task state transition.
#[derive(Clone, Debug, PartialEq)]
pub enum TransitionOutcome {
    /// The transition was applied.
    Applied(ExecutionTaskRecord),
    /// A run wait transition was applied.
    RunApplied(ExecutionRunRecord),
    /// The requested target state already exists for the same generation.
    AlreadyApplied(ExecutionTaskRecord),
    /// The requested run wait state already exists.
    RunAlreadyApplied(ExecutionRunRecord),
    /// No visible task exists.
    NotFound,
    /// Source status or generation did not match.
    Rejected(TransitionRejection),
}

/// Stable reason a task transition was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionRejection {
    /// The supplied generation is stale or otherwise mismatched.
    GenerationMismatch,
    /// The task source status does not permit the operation.
    InvalidTaskStatus,
    /// The run status does not permit the operation.
    InvalidRunStatus,
    /// The approved run deadline elapsed before redispatch admission.
    DeadlineElapsed,
    /// The current run envelope has no remaining resource admission.
    BudgetExceeded,
    /// A counter increment overflowed its persisted representation.
    CounterOverflow,
}

/// Resume operation applied to one waiting or retryable task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeKind {
    /// Resume a waiting-input task, preserving attempt and incrementing generation.
    Input,
    /// Dispatch a retry, incrementing both attempt and generation.
    Retry,
}

/// Result of recording one task outcome message.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskOutcomeWrite {
    /// The outcome was accepted and reconciled into the current projection.
    Applied {
        /// Updated run projection committed in the same transaction.
        run: ExecutionRunRecord,
        /// Updated task projection.
        task: ExecutionTaskRecord,
        /// Whether this write caused or retained a budget overrun.
        budget_overrun: bool,
    },
    /// The same generation and outcome were already accepted durably.
    Replayed {
        /// Current persisted run projection associated with the first accepted outcome.
        run: ExecutionRunRecord,
        /// Current task projection recovered from the accepted audit entry.
        task: ExecutionTaskRecord,
        /// Current persisted run overrun flag associated with the handoff.
        budget_overrun: bool,
    },
    /// The message was appended to audit history but rejected from current state.
    Rejected {
        /// Updated task containing the rejected audit entry.
        task: ExecutionTaskRecord,
        /// Stable rejection reason.
        reason: TaskOutcomeRejection,
    },
    /// No visible task exists.
    NotFound,
}

/// Stable reason an outcome message was audit-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcomeRejection {
    /// The received generation is not current.
    StaleGeneration,
    /// The task is already terminal.
    TerminalTask,
    /// The owning run is cancelled or otherwise terminal.
    TerminalRun,
    /// The task is not currently running.
    InvalidTaskStatus,
    /// Reported cumulative usage moved backward.
    NonCumulativeUsage,
    /// The outcome schema version is unsupported.
    UnsupportedSchemaVersion,
}

/// Compiler-validated amendment data persisted under a revision fence.
#[derive(Clone, Debug)]
pub struct ValidatedAmendment {
    /// Exact compiler-validated patch.
    pub amendment: PlanAmendment,
    /// Domain-separated hash of the patch.
    pub amendment_hash: ExecutionHash,
    /// Replacement canonical active plan.
    pub active_plan: CanonicalExecutionPlan,
    /// Mapping from added/replaced node IDs to unresolved goal requirements.
    pub requirement_mapping: BTreeMap<String, Vec<String>>,
    /// Waiting-replan task superseded by this amendment.
    pub superseded_task_id: ExecutionTaskId,
}

/// Result of an amendment append.
#[derive(Clone, Debug, PartialEq)]
pub struct AmendmentCommit {
    /// Run projection carrying the committed wake epoch and revision.
    pub run: ExecutionRunRecord,
    /// Stable task workflows superseded or terminalized by the amendment.
    pub task_ids_to_release: Vec<ExecutionTaskId>,
}

/// Persisted run-state evidence returned only by a first-applied mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionRunTransitionEvidence {
    /// Locked run status before the transaction committed.
    pub prior_status: ExecutionRunStatus,
    /// Persisted run status after the transaction committed.
    pub status: ExecutionRunStatus,
    /// Database-owned queue timestamp.
    pub queued_at: Option<DateTime<Utc>>,
    /// Database-owned first-start timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Persisted outstanding reservation after the transition.
    pub reserved: ExecutionEstimate,
    /// Persisted reconciled usage after the transition.
    pub consumed: ExecutionEstimate,
    /// Persisted terminal coverage evidence, when terminal.
    pub terminal_evidence: Option<ExecutionTerminalEvidence>,
    /// Persisted closed terminal reason, when terminal.
    pub terminal_reason: Option<ExecutionTerminalReason>,
}

/// Persisted task-state evidence returned only by a first-applied mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTaskTransitionEvidence {
    /// Locked task status before the transaction committed.
    pub prior_status: ExecutionTaskStatus,
    /// Persisted task status after the transaction committed.
    pub status: ExecutionTaskStatus,
    /// Persisted closed logical task kind.
    pub kind: LogicalTaskKind,
    /// Database-owned task creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Database-owned last-mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// Database-owned first-start timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Database-owned terminal timestamp.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Complete metric evidence for one first-applied run mutation transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionMutationMetricEvidence {
    /// Actual run transition committed by the transaction.
    pub run: ExecutionRunTransitionEvidence,
    /// Actual task transitions committed by the transaction.
    pub tasks: Vec<ExecutionTaskTransitionEvidence>,
}

/// Result of checking persisted amendment identity before current-revision validation.
#[derive(Clone, Debug, PartialEq)]
pub enum AmendmentReplayOutcome {
    /// No matching committed amendment exists and the requested base revision is current.
    NotApplied,
    /// The exact revision/hash/audit identity was already committed.
    Replayed(Box<AmendmentCommit>),
    /// No visible run exists.
    NotFound,
    /// A different revision or amendment occupies the requested durable identity.
    Conflict,
}

/// Result of an amendment append.
#[derive(Clone, Debug, PartialEq)]
pub enum AmendmentWrite {
    /// The revision was fenced, history appended, and waiting task superseded.
    Applied {
        /// Durable mutation and workflow-handoff result.
        commit: Box<AmendmentCommit>,
        /// First-apply-only persisted transition evidence.
        metrics: Box<ExecutionMutationMetricEvidence>,
    },
    /// The same revision/hash/audit identity was already committed.
    Replayed(Box<AmendmentCommit>),
    /// No visible run exists.
    NotFound,
    /// Current revision, status, plan, or superseded task did not match.
    Conflict,
}

/// Result of cancelling one run.
#[derive(Clone, Debug, PartialEq)]
pub struct CancellationCommit {
    /// Cancelled run carrying the persisted wake epoch.
    pub run: ExecutionRunRecord,
    /// Stable task workflows that must receive terminal cancellation.
    pub task_ids_to_release: Vec<ExecutionTaskId>,
}

/// Request to atomically cancel one run with a complete terminal replay identity.
#[derive(Clone, Debug, PartialEq)]
pub struct CancellationRequest {
    /// Human-readable caller cancellation reason.
    pub reason: String,
    /// Typed cancellation cause and coverage computed from the observed projection.
    pub terminal_evidence: ExecutionTerminalEvidence,
}

/// Result of cancelling one run.
#[derive(Clone, Debug, PartialEq)]
pub enum CancellationOutcome {
    /// The run and all nonterminal tasks were cancelled atomically.
    Cancelled {
        /// Durable mutation and workflow-handoff result.
        commit: Box<CancellationCommit>,
        /// First-apply-only persisted transition evidence.
        metrics: Box<ExecutionMutationMetricEvidence>,
    },
    /// The exact reason was already committed with the same task handoff set.
    Replayed(Box<CancellationCommit>),
    /// No visible run exists.
    NotFound,
    /// A different terminal outcome already exists.
    Conflict,
}

/// Cursor for stable ascending task pagination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTaskCursor {
    /// Node identifier of the last returned task.
    pub node_id: String,
    /// Item key of the last returned task.
    pub item_key: String,
    /// Stable task ID of the last returned task.
    pub task_id: ExecutionTaskId,
}

/// Bounded task-list request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionTaskPageRequest {
    /// Requested page size, capped at 1,000. Zero selects the default of 100.
    pub limit: u32,
    /// Exclusive pagination cursor.
    pub cursor: Option<ExecutionTaskCursor>,
}

/// One bounded page of execution tasks.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTaskPage {
    /// Visible tasks in stable ascending order.
    pub tasks: Vec<ExecutionTaskRecord>,
    /// Cursor for the next page, when more tasks exist.
    pub next_cursor: Option<ExecutionTaskCursor>,
}

/// Cursor for stable descending run pagination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionRunCursor {
    /// Creation timestamp of the last returned run.
    pub created_at: DateTime<Utc>,
    /// Stable run identifier of the last returned run.
    pub run_uid: Uuid,
}

/// Bounded run-list request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutionRunPageRequest {
    /// Requested page size, capped at 1,000. Zero selects the default of 100.
    pub limit: u32,
    /// Exclusive descending pagination cursor.
    pub cursor: Option<ExecutionRunCursor>,
}

/// One bounded page of execution runs.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionRunPage {
    /// Visible runs in descending creation order.
    pub runs: Vec<ExecutionRunRecord>,
    /// Cursor for the next page, when more runs exist.
    pub next_cursor: Option<ExecutionRunCursor>,
}

/// Atomic scheduler input loaded from one active run revision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionSchedulingSnapshot {
    /// Complete durable run record.
    pub run: ExecutionRunRecord,
    /// Exact persisted immutable capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact persisted immutable authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Exact persisted immutable instruction-skill revisions.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Current persisted budget ledger.
    pub budget_ledger: BudgetLedger,
    /// Complete ordered task and node projection for the active plan revision.
    pub projection: ExecutionProjection,
}

/// Result of compare-and-set wake acknowledgement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WakeAckOutcome {
    /// The exact current epoch was acknowledged.
    Acknowledged {
        /// Newly persisted processed epoch.
        processed_wake_epoch: u64,
    },
    /// The same epoch was already acknowledged.
    Replayed {
        /// Persisted processed epoch.
        processed_wake_epoch: u64,
    },
    /// A later scheduling mutation occurred and remains unacknowledged.
    Changed {
        /// Current persisted wake epoch.
        current_wake_epoch: u64,
    },
    /// No visible run exists.
    NotFound,
}

/// Result of terminal run finalization.
#[derive(Clone, Debug, PartialEq)]
pub enum FinalizationOutcome {
    /// Terminal state and completion evidence were persisted.
    Finalized(ExecutionRunRecord),
    /// The same terminal projection was already persisted.
    Replayed(ExecutionRunRecord),
    /// No visible run exists.
    NotFound,
    /// Revision, status, or completion evaluation did not match.
    Conflict,
}

/// Optimistically fenced request to atomically persist one terminal run projection.
#[derive(Clone, Debug, PartialEq)]
pub struct RunFinalizationRequest {
    /// Run to finalize.
    pub run_uid: Uuid,
    /// Active plan revision used for completion evaluation.
    pub expected_revision: u64,
    /// Wake epoch of the structured projection used for completion evaluation.
    pub expected_wake_epoch: u64,
    /// Exact terminal projection selected by the scheduler.
    pub terminal_projection: TerminalProjection,
    /// Deterministic completion evaluation over the observed projection.
    pub completion_evaluation: CompletionEvaluation,
    /// Exact typed cause and requirement-count replay identity.
    pub terminal_evidence: ExecutionTerminalEvidence,
    /// Exact normalized terminal reason selected from typed evidence.
    pub terminal_reason: ExecutionTerminalReason,
}

/// Committed run and originating task projections for a terminal replan stop.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplanStopFinalization {
    /// Finalized partial or blocked run.
    pub run: ExecutionRunRecord,
    /// Cancelled originating waiting-replan task.
    pub task: ExecutionTaskRecord,
    /// Stable task workflows terminalized by the stop decision.
    pub task_ids_to_release: Vec<ExecutionTaskId>,
}

/// Result of atomically cancelling one waiting-replan task and finalizing its run.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplanStopOutcome {
    /// Task cancellation, reservation reconciliation, and run finalization committed.
    Finalized(Box<ReplanStopFinalization>),
    /// The exact generation/revision-fenced stop was already committed.
    Replayed(Box<ReplanStopFinalization>),
    /// No visible run or task exists.
    NotFound,
    /// Run revision, task generation/status, or terminal evidence did not match.
    Conflict,
}

/// Generation- and revision-fenced request for terminal replan-stop finalization.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplanStopRequest {
    /// Run to finalize.
    pub run_uid: Uuid,
    /// Active plan revision observed by the scheduler.
    pub expected_revision: u64,
    /// Wake epoch of the structured projection used for stop evaluation.
    pub expected_wake_epoch: u64,
    /// Waiting-replan task that triggered stop evaluation.
    pub task_id: ExecutionTaskId,
    /// Current generation of the originating waiting task.
    pub expected_generation: u64,
    /// Exact amendment hash whose stop evaluation caused finalization, when amendment-driven.
    pub amendment_hash: Option<ExecutionHash>,
    /// Typed cancellation reason written to every active task.
    pub cancellation_reason: String,
    /// Partial or blocked terminal projection for the run.
    pub terminal_projection: TerminalProjection,
    /// Deterministic completion evidence persisted with the run.
    pub completion_evaluation: CompletionEvaluation,
    /// Exact typed cause and requirement-count replay identity.
    pub terminal_evidence: ExecutionTerminalEvidence,
    /// Exact normalized terminal reason selected from typed evidence.
    pub terminal_reason: ExecutionTerminalReason,
}

/// Result of idempotently persisting one action-review resolution delivery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ActionReviewResolutionWrite {
    /// The review UID was accepted for the current running generation.
    Applied,
    /// The same review UID and generation were already recorded.
    Replayed,
    /// The review was audited but its task generation or status was stale.
    AuditedStale,
    /// No visible task exists.
    NotFound,
}

/// Scoped repository for durable execution runs and logical tasks.
#[derive(Clone, Debug)]
pub struct ExecutionRepository {
    pool: PgPool,
}

impl ExecutionRepository {
    /// Creates a repository over the shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DbBudgetLimit {
    max_cost_microusd: Option<i64>,
    max_tokens: Option<i64>,
    max_tasks: Option<i64>,
    max_tool_calls: Option<i64>,
    max_retrieved_bytes: Option<i64>,
    deadline_at: Option<DateTime<Utc>>,
}

impl TryFrom<&ExecutionBudgetLimit> for DbBudgetLimit {
    type Error = Error;

    fn try_from(value: &ExecutionBudgetLimit) -> Result<Self> {
        Ok(Self {
            max_cost_microusd: to_optional_i64(value.max_cost_microusd, "max cost")?,
            max_tokens: to_optional_i64(value.max_tokens, "max tokens")?,
            max_tasks: to_optional_i64(value.max_tasks, "max tasks")?,
            max_tool_calls: to_optional_i64(value.max_tool_calls, "max tool calls")?,
            max_retrieved_bytes: to_optional_i64(value.max_retrieved_bytes, "max retrieved bytes")?,
            deadline_at: value.deadline_at,
        })
    }
}

fn to_optional_i64(value: Option<u64>, field: &str) -> Result<Option<i64>> {
    value.map(|value| to_i64(value, field)).transpose()
}

fn to_i64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::InvalidRepositoryInput {
        message: format!("{field} exceeds PostgreSQL BIGINT"),
    })
}

fn to_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidRepositoryData {
        message: format!("{field} is negative"),
    })
}

fn to_u32(value: i32, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::InvalidRepositoryData {
        message: format!("{field} is negative"),
    })
}

fn storage_error(error: moa_core::error::MoaError) -> Error {
    Error::Storage {
        message: error.to_string(),
    }
}

fn sqlx_error(error: sqlx::Error) -> Error {
    Error::Storage {
        message: error.to_string(),
    }
}

fn row_error(error: sqlx::Error) -> Error {
    Error::InvalidRepositoryData {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests;
