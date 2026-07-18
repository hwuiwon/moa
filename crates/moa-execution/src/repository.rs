//! Scoped PostgreSQL persistence for durable execution runs and logical tasks.

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
    budget::BudgetLedger,
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash, amendment_hash,
    },
    compiler::CanonicalExecutionPlan,
    completion::{
        CompletionEvaluation, CompletionStatus, cancellation_terminal_evidence,
        execution_terminal_reason, terminal_evidence_from_evaluation,
    },
    replan::failure_fingerprint,
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionRunStatus, ExecutionSourceKind,
        ExecutionTaskId, ExecutionTaskProjection, ExecutionTaskStatus, ExecutionTerminalCause,
        ExecutionTerminalEvidence, ExecutionTerminalReason, FailureFingerprintInput, LogicalTask,
        LogicalTaskKind, TerminalProjection, WaitingReason,
    },
    wire::{
        ExecutionActionReviewResolution, ExecutionPlanningContextSnapshot,
        ExecutionTerminalDelivery, PinnedInstructionSkill, execution_terminal_delivery_from_state,
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
    /// Normalized source cohort persisted by V000337.
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

    /// Inserts or exactly replays one immutable origin-bound planning context.
    pub async fn create_planning_context(
        &self,
        scope: ExecutionScope,
        new_context: NewExecutionPlanningContext,
    ) -> Result<PlanningContextWriteOutcome> {
        let snapshot = &new_context.snapshot;
        if snapshot.schema_version != 1
            || !scope.permits_owner(snapshot.tenant_id, snapshot.contact_id)
            || snapshot
                .contact_id
                .is_some_and(|contact_id| contact_id.0.is_nil())
        {
            return Err(Error::InvalidRepositoryInput {
                message: "planning context scope or schema version is invalid".to_string(),
            });
        }
        let sequence = to_i64(
            snapshot.originating_user_sequence_num,
            "originating user sequence",
        )?;
        let snapshot_value = serde_json::to_value(snapshot)?;
        let planning_context_uid = Uuid::now_v7();
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(CREATE_PLANNING_CONTEXT_SQL)
            .bind(planning_context_uid)
            .bind(snapshot.tenant_id.0)
            .bind(snapshot.contact_id.map(|value| value.0))
            .bind(snapshot.session_id.0)
            .bind(sequence)
            .bind(&snapshot.originating_user_event_hash)
            .bind(snapshot.owner_user_id.as_str())
            .bind(new_context.planning_context_hash.to_string())
            .bind(snapshot_value)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = row {
            PlanningContextWriteOutcome::Created(planning_context_from_row(&row)?)
        } else {
            let row = sqlx::query(LOAD_PLANNING_CONTEXT_BY_ORIGIN_SQL)
                .bind(snapshot.tenant_id.0)
                .bind(snapshot.session_id.0)
                .bind(sequence)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
                .ok_or_else(|| Error::Storage {
                    message: "planning-context origin conflict had no visible row".to_string(),
                })?;
            let existing = planning_context_from_row(&row)?;
            if existing.snapshot == new_context.snapshot
                && existing.planning_context_hash == new_context.planning_context_hash
            {
                PlanningContextWriteOutcome::Replayed(existing)
            } else {
                PlanningContextWriteOutcome::Conflict
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Loads one visible immutable planning context by durable identifier.
    pub async fn load_planning_context(
        &self,
        scope: ExecutionScope,
        planning_context_uid: Uuid,
    ) -> Result<Option<ExecutionPlanningContextRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_PLANNING_CONTEXT_SQL)
            .bind(planning_context_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(planning_context_from_row).transpose()
    }

    /// Inserts or exactly replays one normalized V000337 route-audit row.
    pub async fn write_route_audit(
        &self,
        scope: ExecutionScope,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> Result<RouteAuditWriteOutcome> {
        validate_audit_scope(scope, envelope)?;
        let (
            ExecutionPlanningAuditPayload::Route {
                stage,
                decision,
                strategy,
                provenance,
                accepted_at,
            },
            Some(session_id),
            Some(originating_sequence),
        ) = (
            &envelope.payload,
            envelope.session_id,
            envelope.originating_sequence,
        )
        else {
            return Err(Error::InvalidRepositoryInput {
                message: "route audit requires a session-bound route payload".to_string(),
            });
        };
        let audit_uid = route_audit_uid(
            envelope.tenant_id,
            envelope.contact_id,
            session_id,
            originating_sequence,
            *stage,
        )?;
        let originating_sequence_db = to_i64(originating_sequence, "originating sequence")?;
        let confidence_bps = provenance
            .confidence_bps
            .map(i16::try_from)
            .transpose()
            .map_err(|_| Error::InvalidRepositoryInput {
                message: "route confidence exceeds SMALLINT".to_string(),
            })?;
        let missing_input_count = i16::from(provenance.missing_input_count);
        let mut conn = scope.begin(&self.pool).await?;
        let inserted = sqlx::query(INSERT_ROUTE_AUDIT_SQL)
            .bind(audit_uid)
            .bind(envelope.tenant_id.0)
            .bind(envelope.contact_id.map(|value| value.0))
            .bind(session_id.0)
            .bind(originating_sequence_db)
            .bind(route_stage_label(*stage))
            .bind(route_decision_label(*decision))
            .bind(strategy.map(execution_strategy_label))
            .bind(route_source_label(provenance.source))
            .bind(route_classifier_outcome_label(
                provenance.classifier_outcome,
            ))
            .bind(provenance.provider_model.as_deref())
            .bind(provenance.prompt_version.as_deref())
            .bind(provenance.objective_hash.as_str())
            .bind(provenance.response_hash.as_deref())
            .bind(confidence_bps)
            .bind(missing_input_count)
            .bind(to_i64(
                provenance.usage.input_tokens_uncached,
                "route uncached input tokens",
            )?)
            .bind(to_i64(
                provenance.usage.input_tokens_cache_write,
                "route cache-write input tokens",
            )?)
            .bind(to_i64(
                provenance.usage.input_tokens_cache_read,
                "route cache-read input tokens",
            )?)
            .bind(to_i64(
                provenance.usage.output_tokens,
                "route output tokens",
            )?)
            .bind(to_i64(provenance.cost_microusd, "route cost")?)
            .bind(to_i64(provenance.duration_micros, "route duration")?)
            .bind(*accepted_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = inserted {
            RouteAuditWriteOutcome::Applied(route_audit_from_row(&row)?.evidence)
        } else {
            let row = sqlx::query(LOAD_ROUTE_AUDIT_SQL)
                .bind(envelope.tenant_id.0)
                .bind(envelope.contact_id.map(|value| value.0))
                .bind(session_id.0)
                .bind(originating_sequence_db)
                .bind(route_stage_label(*stage))
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(RouteAuditWriteOutcome::Conflict { audit_uid });
            };
            let persisted = route_audit_from_row(&row)?;
            if persisted.audit_uid == audit_uid
                && persisted.stage == *stage
                && persisted.evidence.decision == *decision
                && persisted.evidence.strategy == *strategy
                && route_provenance_semantically_equal(&persisted.evidence.provenance, provenance)
            {
                RouteAuditWriteOutcome::Replayed(persisted.evidence)
            } else {
                RouteAuditWriteOutcome::Conflict { audit_uid }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Inserts or exactly replays one normalized V000337 planner-call audit row.
    pub async fn write_planner_call_audit(
        &self,
        scope: ExecutionScope,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> Result<PlannerCallAuditWriteOutcome> {
        validate_audit_scope(scope, envelope)?;
        let (
            ExecutionPlanningAuditPayload::PlannerCall {
                call_kind,
                call_ordinal,
                run_uid,
                plan_revision,
                outcome,
                provider_model,
                prompt_version,
                candidate_hash,
                candidate_json,
                compiler_report,
                duration_micros,
                created_at,
            },
            Some(session_id),
            Some(originating_sequence),
        ) = (
            &envelope.payload,
            envelope.session_id,
            envelope.originating_sequence,
        )
        else {
            return Err(Error::InvalidRepositoryInput {
                message: "planner audit requires a session-bound planner-call payload".to_string(),
            });
        };
        let audit_uid = planner_audit_uid(
            envelope.tenant_id,
            envelope.contact_id,
            session_id,
            originating_sequence,
            *run_uid,
            *plan_revision,
            *call_kind,
            *call_ordinal,
        )?;
        let originating_sequence_db = to_i64(originating_sequence, "originating sequence")?;
        let plan_revision_db = plan_revision
            .map(|value| to_i64(value, "plan revision"))
            .transpose()?;
        let duration_micros_db = to_i64(*duration_micros, "planner duration")?;
        let call_ordinal_db = i16::from(*call_ordinal);
        let mut conn = scope.begin(&self.pool).await?;
        let inserted = sqlx::query(INSERT_PLANNER_AUDIT_SQL)
            .bind(audit_uid)
            .bind(envelope.tenant_id.0)
            .bind(envelope.contact_id.map(|value| value.0))
            .bind(session_id.0)
            .bind(originating_sequence_db)
            .bind(*run_uid)
            .bind(plan_revision_db)
            .bind(planner_call_label(*call_kind))
            .bind(call_ordinal_db)
            .bind(planner_outcome_label(*outcome))
            .bind(provider_model)
            .bind(prompt_version)
            .bind(candidate_hash)
            .bind(candidate_json)
            .bind(compiler_report)
            .bind(duration_micros_db)
            .bind(*created_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = inserted {
            PlannerCallAuditWriteOutcome::Applied(planner_audit_from_row(&row)?.evidence)
        } else {
            let row = sqlx::query(LOAD_PLANNER_AUDIT_SQL)
                .bind(envelope.tenant_id.0)
                .bind(envelope.contact_id.map(|value| value.0))
                .bind(session_id.0)
                .bind(originating_sequence_db)
                .bind(*run_uid)
                .bind(plan_revision_db)
                .bind(planner_call_label(*call_kind))
                .bind(call_ordinal_db)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(PlannerCallAuditWriteOutcome::Conflict { audit_uid });
            };
            let persisted = planner_audit_from_row(&row)?;
            if persisted.semantically_matches(audit_uid, envelope) {
                PlannerCallAuditWriteOutcome::Replayed(persisted.evidence)
            } else {
                PlannerCallAuditWriteOutcome::Conflict { audit_uid }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Inserts or exactly replays one normalized V000337 compiler-audit row.
    pub async fn write_compile_audit(
        &self,
        scope: ExecutionScope,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> Result<CompileAuditWriteOutcome> {
        validate_audit_scope(scope, envelope)?;
        let ExecutionPlanningAuditPayload::Compile {
            source,
            operation_key,
            run_uid,
            plan_revision,
            outcome,
            candidate_hash,
            final_plan_hash,
            validation_report,
            duration_micros,
            created_at,
        } = &envelope.payload
        else {
            return Err(Error::InvalidRepositoryInput {
                message: "compiler audit requires a compile payload".to_string(),
            });
        };
        let audit_uid = compile_audit_uid(
            envelope.tenant_id,
            envelope.contact_id,
            *source,
            operation_key,
        )?;
        let originating_sequence_db = envelope
            .originating_sequence
            .map(|value| to_i64(value, "originating sequence"))
            .transpose()?;
        let plan_revision_db = plan_revision
            .map(|value| to_i64(value, "plan revision"))
            .transpose()?;
        let duration_micros_db = to_i64(*duration_micros, "compile duration")?;
        let mut conn = scope.begin(&self.pool).await?;
        let inserted = sqlx::query(INSERT_COMPILE_AUDIT_SQL)
            .bind(audit_uid)
            .bind(envelope.tenant_id.0)
            .bind(envelope.contact_id.map(|value| value.0))
            .bind(envelope.session_id.map(|value| value.0))
            .bind(originating_sequence_db)
            .bind(*run_uid)
            .bind(plan_revision_db)
            .bind(compile_source_label(*source))
            .bind(operation_key)
            .bind(compile_outcome_label(*outcome))
            .bind(candidate_hash)
            .bind(final_plan_hash)
            .bind(validation_report)
            .bind(duration_micros_db)
            .bind(*created_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = inserted {
            CompileAuditWriteOutcome::Applied(compile_audit_from_row(&row)?.evidence)
        } else {
            let row = sqlx::query(LOAD_COMPILE_AUDIT_SQL)
                .bind(envelope.tenant_id.0)
                .bind(envelope.contact_id.map(|value| value.0))
                .bind(compile_source_label(*source))
                .bind(operation_key)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(CompileAuditWriteOutcome::Conflict { audit_uid });
            };
            let persisted = compile_audit_from_row(&row)?;
            if persisted.semantically_matches(audit_uid, envelope) {
                CompileAuditWriteOutcome::Replayed(persisted.evidence)
            } else {
                CompileAuditWriteOutcome::Conflict { audit_uid }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Creates a run or returns the existing row for the same scoped idempotency key.
    pub async fn create_run(
        &self,
        scope: ExecutionScope,
        new_run: NewExecutionRun,
    ) -> Result<ExecutionRunRecord> {
        validate_new_run(scope, &new_run)?;
        let budget = DbBudgetLimit::try_from(&new_run.approved_budget)?;
        let run_uid = Uuid::now_v7();
        let plan_value = serde_json::to_value(&new_run.plan)?;
        let goal_value = serde_json::to_value(&new_run.goal)?;
        let catalog_value = serde_json::to_value(&new_run.catalog)?;
        let authorization_value = serde_json::to_value(&new_run.authorization)?;
        let pinned_skills_value = serde_json::to_value(&new_run.pinned_instruction_skills)?;
        let source_provenance_value = serde_json::to_value(&new_run.source_provenance)?;
        let source_fields = normalized_source_fields(&new_run.source_provenance);
        let originating_user_sequence_num = to_i64(
            new_run.originating_user_sequence_num,
            "originating user sequence",
        )?;
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(CREATE_RUN_SQL)
            .bind(run_uid)
            .bind(new_run.tenant_id.0)
            .bind(new_run.contact_id.map(|value| value.0))
            .bind(new_run.session_id.0)
            .bind(originating_user_sequence_num)
            .bind(new_run.planning_context_uid)
            .bind(new_run.planning_context_hash.to_string())
            .bind(new_run.owner_user_id.as_str())
            .bind(goal_value)
            .bind(&plan_value)
            .bind(&plan_value)
            .bind(new_run.plan.plan_hash.to_string())
            .bind(new_run.plan.plan_hash.to_string())
            .bind(catalog_value)
            .bind(authorization_value)
            .bind(pinned_skills_value)
            .bind(source_provenance_value)
            .bind(source_fields.kind.as_str())
            .bind(source_fields.skill_template_ref)
            .bind(source_fields.skill_template_revision_uid)
            .bind(new_run.input)
            .bind(new_run.status.as_str())
            .bind(budget.max_cost_microusd)
            .bind(budget.max_tokens)
            .bind(budget.max_tasks)
            .bind(budget.max_tool_calls)
            .bind(budget.max_retrieved_bytes)
            .bind(budget.deadline_at)
            .bind(0_i64)
            .bind(new_run.idempotency_key.as_deref())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;

        let record = if let Some(row) = row {
            run_from_row(&row)?
        } else if let Some(idempotency_key) = new_run.idempotency_key.as_deref() {
            let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_SQL)
                .bind(new_run.tenant_id.0)
                .bind(new_run.contact_id.map(|value| value.0))
                .bind(idempotency_key)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
                .ok_or_else(|| Error::Storage {
                    message: "idempotent run insert conflicted without a visible existing row"
                        .to_string(),
                })?;
            run_from_row(&row)?
        } else {
            return Err(Error::Storage {
                message: "execution run insert conflicted without an idempotency key".to_string(),
            });
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Loads one visible execution run.
    pub async fn load_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionRunRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads one visible task under its owning run and stable task ID.
    pub async fn load_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
    ) -> Result<Option<ExecutionTaskRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(task_from_row).transpose()
    }

    /// Loads a visible run for one scope-local idempotency key.
    pub async fn load_run_by_idempotency_key(
        &self,
        scope: ExecutionScope,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        idempotency_key: &str,
    ) -> Result<Option<ExecutionRunRecord>> {
        if !scope.permits_owner(tenant_id, contact_id) {
            return Err(Error::InvalidRepositoryInput {
                message: "idempotency lookup owner does not match repository scope".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_SQL)
            .bind(tenant_id.0)
            .bind(contact_id.map(|value| value.0))
            .bind(idempotency_key)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Lists one bounded, stable page of visible execution runs.
    pub async fn list_runs(
        &self,
        scope: ExecutionScope,
        page: ExecutionRunPageRequest,
    ) -> Result<ExecutionRunPage> {
        let limit = if page.limit == 0 {
            DEFAULT_RUN_PAGE_LIMIT
        } else {
            page.limit.min(MAX_RUN_PAGE_LIMIT)
        };
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(LIST_RUNS_SQL)
            .bind(page.cursor.map(|cursor| cursor.created_at))
            .bind(page.cursor.map(|cursor| cursor.run_uid))
            .bind(i64::from(limit) + 1)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let mut runs = rows.iter().map(run_from_row).collect::<Result<Vec<_>>>()?;
        let has_more = runs.len() > limit as usize;
        if has_more {
            let _ = runs.pop();
        }
        let next_cursor = if has_more {
            runs.last().map(|run| ExecutionRunCursor {
                created_at: run.created_at,
                run_uid: run.run_uid,
            })
        } else {
            None
        };
        Ok(ExecutionRunPage { runs, next_cursor })
    }

    /// Loads one repeatable-read scheduling snapshot with its complete ordered task projection.
    pub async fn load_scheduling_snapshot(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionSchedulingSnapshot>> {
        let mut conn = self.pool.begin().await.map_err(sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        install_execution_scope(&mut conn, scope).await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        let Some(row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(sqlx_error)?;
            return Ok(None);
        };
        let run = run_from_row(&row)?;
        let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
            .bind(run_uid)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(sqlx_error)?;
        let tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let projection = scheduling_projection(&run, &tasks);
        Ok(Some(ExecutionSchedulingSnapshot {
            catalog: run.catalog.clone(),
            authorization: run.authorization.clone(),
            pinned_instruction_skills: run.pinned_instruction_skills.clone(),
            budget_ledger: budget_ledger(&run),
            run,
            projection,
        }))
    }

    /// Loads one terminal run and derives its compact session delivery from the same snapshot.
    pub async fn load_terminal_delivery(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionTerminalDelivery>> {
        let Some(snapshot) = self.load_scheduling_snapshot(scope, run_uid).await? else {
            return Ok(None);
        };
        execution_terminal_delivery_from_state(&snapshot.run, &snapshot.projection).map(Some)
    }

    /// Acknowledges only the exact current wake epoch, preserving any later wake.
    pub async fn ack_run_wake(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_wake_epoch: u64,
    ) -> Result<WakeAckOutcome> {
        let expected = to_i64(expected_wake_epoch, "wake epoch")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(WakeAckOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        let outcome = if run.wake_epoch != expected_wake_epoch {
            if expected_wake_epoch <= run.processed_wake_epoch {
                WakeAckOutcome::Replayed {
                    processed_wake_epoch: run.processed_wake_epoch,
                }
            } else {
                WakeAckOutcome::Changed {
                    current_wake_epoch: run.wake_epoch,
                }
            }
        } else if run.processed_wake_epoch >= expected_wake_epoch {
            WakeAckOutcome::Replayed {
                processed_wake_epoch: run.processed_wake_epoch,
            }
        } else {
            let updated = sqlx::query(
                "UPDATE moa.execution_run SET processed_wake_epoch = $2, updated_at = NOW() \
                 WHERE run_uid = $1 AND wake_epoch = $2 AND processed_wake_epoch < $2",
            )
            .bind(run_uid)
            .bind(expected)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if updated.rows_affected() == 1 {
                WakeAckOutcome::Acknowledged {
                    processed_wake_epoch: expected_wake_epoch,
                }
            } else {
                return Err(Error::Storage {
                    message: "wake acknowledgement lost its locked compare-and-set".to_string(),
                });
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }
}

impl ExecutionRepository {
    /// Atomically reserves all five resource dimensions for one pending task.
    pub async fn reserve_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
    ) -> Result<ReservationOutcome> {
        let generation_db = to_i64(generation, "task generation")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if task.generation != generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Rejected(
                ReservationRejection::GenerationMismatch,
            ));
        }
        if let Some(rejection) = terminal_reservation_rejection(&task) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::AlreadyTerminalized(Box::new(
                ReservationTerminalization {
                    run,
                    task,
                    rejection,
                },
            )));
        }
        if task.status == ExecutionTaskStatus::Reserved {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::AlreadyReserved(task));
        }
        if task.status != ExecutionTaskStatus::Pending {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Rejected(
                ReservationRejection::InvalidTaskStatus,
            ));
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Rejected(
                ReservationRejection::InvalidRunStatus,
            ));
        }
        if run
            .approved_budget
            .deadline_at
            .is_some_and(|deadline| Utc::now() > deadline)
        {
            let terminalized = terminalize_reservation_rejection(
                &mut conn,
                &run,
                &task,
                ReservationRejection::DeadlineElapsed,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Terminalized(Box::new(terminalized)));
        }
        let estimate = DbEstimate::try_from(task.estimate)?;
        let run_updated = sqlx::query(RESERVE_RUN_BUDGET_SQL)
            .bind(run_uid)
            .bind(estimate.cost_microusd)
            .bind(estimate.tokens)
            .bind(estimate.tasks)
            .bind(estimate.tool_calls)
            .bind(estimate.retrieved_bytes)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        if run_updated.rows_affected() != 1 {
            let terminalized = terminalize_reservation_rejection(
                &mut conn,
                &run,
                &task,
                ReservationRejection::BudgetExceeded,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Terminalized(Box::new(terminalized)));
        }

        let row = sqlx::query(RESERVE_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(generation_db)
            .bind(estimate.cost_microusd)
            .bind(estimate.tokens)
            .bind(estimate.tasks)
            .bind(estimate.tool_calls)
            .bind(estimate.retrieved_bytes)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ReservationOutcome::Reserved(task))
    }

    /// Marks one reserved task running under its current generation fence.
    pub async fn mark_task_running(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
    ) -> Result<TransitionOutcome> {
        let generation_db = to_i64(generation, "task generation")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if task.generation != generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::GenerationMismatch,
            ));
        }
        if task.status == ExecutionTaskStatus::Running {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::AlreadyApplied(task));
        }
        if task.status != ExecutionTaskStatus::Reserved {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidTaskStatus,
            ));
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }

        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = CASE WHEN status = 'queued' THEN 'running' ELSE status END, \
                 started_at = COALESCE(started_at, NOW()), \
                 wake_epoch = wake_epoch + 1, updated_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let row = sqlx::query(MARK_TASK_RUNNING_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(generation_db)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::Applied(task))
    }

    /// Resumes input-waiting work or dispatches a retry under a new generation.
    pub async fn resume_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        kind: ResumeKind,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(scope, run_uid, task_id, generation, kind, None)
            .await
    }

    /// Resumes one waiting-input task and atomically appends the exact supplied payload.
    pub async fn resume_task_with_input(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        input: Value,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(
            scope,
            run_uid,
            task_id,
            generation,
            ResumeKind::Input,
            Some(input),
        )
        .await
    }

    /// Dispatches one retry when the persisted retry policy has attempts remaining.
    pub async fn retry_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(scope, run_uid, task_id, generation, ResumeKind::Retry, None)
            .await
    }

    async fn resume_task_inner(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        kind: ResumeKind,
        resume_input: Option<Value>,
    ) -> Result<TransitionOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        let history_kind = match kind {
            ResumeKind::Input => "input_resume",
            ResumeKind::Retry => "retry",
        };
        if redispatch_is_exact_replay(&task, history_kind, generation, resume_input.as_ref()) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::AlreadyApplied(task));
        }
        if task.generation != generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::GenerationMismatch,
            ));
        }
        if kind == ResumeKind::Retry && task.attempt >= task.retry.max_attempts {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidTaskStatus,
            ));
        }
        let retry_attempt = task.attempt.checked_add(1);
        let (expected_status, allowed_run_status, next_attempt) = match kind {
            ResumeKind::Input => (
                ExecutionTaskStatus::WaitingInput,
                matches!(
                    run.status,
                    ExecutionRunStatus::WaitingInput | ExecutionRunStatus::Running
                ),
                task.attempt,
            ),
            ResumeKind::Retry => (
                ExecutionTaskStatus::Running,
                run.status == ExecutionRunStatus::Running,
                match retry_attempt {
                    Some(attempt) => attempt,
                    None => {
                        conn.commit().await.map_err(storage_error)?;
                        return Ok(TransitionOutcome::Rejected(
                            TransitionRejection::CounterOverflow,
                        ));
                    }
                },
            ),
        };
        if task.status != expected_status {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidTaskStatus,
            ));
        }
        if !allowed_run_status {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }
        let admission_rejection = if run
            .approved_budget
            .deadline_at
            .is_some_and(|deadline| Utc::now() > deadline)
        {
            Some((
                moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
                TransitionRejection::DeadlineElapsed,
            ))
        } else if resume_budget_exhausted(&run, &task) {
            Some((
                moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded,
                TransitionRejection::BudgetExceeded,
            ))
        } else {
            None
        };
        let Some(next_generation) = generation.checked_add(1) else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::CounterOverflow,
            ));
        };
        let history = json!({
            "kind": history_kind,
            "requested_generation": generation,
            "attempt": next_attempt,
            "generation": next_generation,
            "admission_rejection": admission_rejection
                .as_ref()
                .map(|(_, reason)| format!("{reason:?}")),
        });

        let row = sqlx::query(RESUME_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(task.status.as_str())
            .bind(to_i64(generation, "task generation")?)
            .bind(
                i32::try_from(next_attempt).map_err(|_| Error::InvalidRepositoryInput {
                    message: "task attempt exceeds PostgreSQL INTEGER".to_string(),
                })?,
            )
            .bind(to_i64(next_generation, "next task generation")?)
            .bind(history)
            .bind(resume_input)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        if let Some((class, reason)) = admission_rejection {
            let task = terminalize_redispatch_rejection(
                &mut conn,
                &run,
                &task,
                history_kind,
                class,
                reason,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Applied(task));
        }
        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = CASE WHEN status IN ('waiting_input', 'waiting_replan') \
                               THEN 'running' ELSE status END, \
                 waiting_reasons = '[]'::JSONB, wake_epoch = wake_epoch + 1, \
                 updated_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::Applied(task))
    }

    /// Lists one bounded, stable page of visible tasks for a run.
    pub async fn list_tasks(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        page: ExecutionTaskPageRequest,
    ) -> Result<ExecutionTaskPage> {
        let limit = if page.limit == 0 {
            DEFAULT_TASK_PAGE_LIMIT
        } else {
            page.limit.min(MAX_TASK_PAGE_LIMIT)
        };
        let fetch_limit = i64::from(limit) + 1;
        let mut conn = scope.begin(&self.pool).await?;
        let cursor_node_id = page.cursor.as_ref().map(|cursor| cursor.node_id.as_str());
        let cursor_item_key = page.cursor.as_ref().map(|cursor| cursor.item_key.as_str());
        let cursor_task_id = page.cursor.as_ref().map(|cursor| cursor.task_id.as_uuid());
        let rows = sqlx::query(LIST_TASKS_SQL)
            .bind(run_uid)
            .bind(cursor_node_id)
            .bind(cursor_item_key)
            .bind(cursor_task_id)
            .bind(fetch_limit)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let mut tasks = rows.iter().map(task_from_row).collect::<Result<Vec<_>>>()?;
        let has_more = tasks.len() > limit as usize;
        if has_more {
            let _ = tasks.pop();
        }
        let next_cursor = if has_more {
            tasks.last().map(|task| ExecutionTaskCursor {
                node_id: task.node_id.clone(),
                item_key: task.item_key.clone(),
                task_id: task.task_id,
            })
        } else {
            None
        };
        Ok(ExecutionTaskPage { tasks, next_cursor })
    }

    /// Transitions a run into one scheduler wait state under a source-status fence.
    pub async fn transition_run_wait(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_status: ExecutionRunStatus,
        waiting_status: ExecutionRunStatus,
    ) -> Result<TransitionOutcome> {
        self.transition_run_wait_with_reasons(
            scope,
            run_uid,
            expected_status,
            waiting_status,
            Vec::new(),
        )
        .await
    }

    /// Transitions a run and persists the exact scheduler wait reasons atomically.
    pub async fn transition_run_wait_with_reasons(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_status: ExecutionRunStatus,
        waiting_status: ExecutionRunStatus,
        waiting_reasons: Vec<WaitingReason>,
    ) -> Result<TransitionOutcome> {
        if !matches!(
            waiting_status,
            ExecutionRunStatus::WaitingInput
                | ExecutionRunStatus::WaitingReview
                | ExecutionRunStatus::WaitingReplan
                | ExecutionRunStatus::Running
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "run wait target must be running or one waiting status".to_string(),
            });
        }
        let waiting_value = serde_json::to_value(&waiting_reasons)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.status == waiting_status && current.waiting_reasons == waiting_reasons {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::RunAlreadyApplied(current));
        }
        if current.status != expected_status || current.status.is_terminal() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }
        if current.status == ExecutionRunStatus::Queued
            && waiting_status != ExecutionRunStatus::Running
        {
            sqlx::query(
                "UPDATE moa.execution_run SET status = 'running', \
                 started_at = COALESCE(started_at, NOW()), updated_at = NOW() \
                 WHERE run_uid = $1 AND status = 'queued'",
            )
            .bind(run_uid)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        }
        let row = sqlx::query(
            "UPDATE moa.execution_run \
             SET status = $2, waiting_reasons = $3, wake_epoch = wake_epoch + 1, \
                 started_at = CASE WHEN $2 = 'running' THEN COALESCE(started_at, NOW()) \
                                   ELSE started_at END, \
                 updated_at = NOW() \
             WHERE run_uid = $1 \
             RETURNING *",
        )
        .bind(run_uid)
        .bind(waiting_status.as_str())
        .bind(waiting_value)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::RunApplied(run))
    }

    /// Records a generation-fenced zero- or nonzero-usage outcome for a parked external wait.
    pub async fn complete_external_wait(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        outcome: ExecutionTaskOutcome,
    ) -> Result<TaskOutcomeWrite> {
        self.record_task_outcome(scope, run_uid, task_id, generation, outcome)
            .await
    }

    /// Idempotently audits one action-review resolution under its task generation fence.
    pub async fn record_action_review_resolution(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        review_uid: Uuid,
        resolution: &ExecutionActionReviewResolution,
    ) -> Result<ActionReviewResolutionWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ActionReviewResolutionWrite::NotFound);
        };
        let task = task_from_row(&row)?;
        let review_uid_text = review_uid.to_string();
        if task.outcome_audit.iter().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("execution_action_review_resolution")
                && entry.get("review_uid").and_then(Value::as_str) == Some(review_uid_text.as_str())
                && entry.get("generation").and_then(Value::as_u64) == Some(generation)
        }) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ActionReviewResolutionWrite::Replayed);
        }
        let accepted = task.generation == generation && task.status == ExecutionTaskStatus::Running;
        let audit = json!({
            "kind": "execution_action_review_resolution",
            "review_uid": review_uid,
            "generation": generation,
            "accepted": accepted,
            "resolution": resolution,
            "recorded_at": Utc::now(),
        });
        sqlx::query(APPEND_TASK_OUTCOME_AUDIT_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(audit)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(if accepted {
            ActionReviewResolutionWrite::Applied
        } else {
            ActionReviewResolutionWrite::AuditedStale
        })
    }

    /// Atomically finalizes one active revision with deterministic completion evidence.
    pub async fn finalize_run(
        &self,
        scope: ExecutionScope,
        request: RunFinalizationRequest,
    ) -> Result<FinalizationOutcome> {
        let RunFinalizationRequest {
            run_uid,
            expected_revision,
            expected_wake_epoch,
            terminal_projection,
            completion_evaluation,
            terminal_evidence,
            terminal_reason,
        } = request;
        let expected_status = status_from_completion(completion_evaluation.status);
        if status_from_terminal_projection(&terminal_projection) != expected_status {
            return Err(Error::InvalidRepositoryInput {
                message: "terminal projection and completion evaluation disagree".to_string(),
            });
        }
        let selected_reason = execution_terminal_reason(
            &terminal_evidence.cause,
            &terminal_projection,
            &completion_evaluation,
        )?;
        if terminal_reason != selected_reason {
            return Err(Error::InvalidRepositoryInput {
                message: "selected terminal reason disagrees with typed terminal evidence"
                    .to_string(),
            });
        }
        let output = terminal_projection_output(&terminal_projection);
        let checks = serde_json::to_value(&completion_evaluation.checks)?;
        let gaps = serde_json::to_value(&completion_evaluation.gaps)?;
        let terminal_cause = serde_json::to_value(&terminal_evidence.cause)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.status.is_terminal() {
            let replay = current.plan_revision == expected_revision
                && current.status == expected_status
                && current.output == output
                && serde_json::to_value(&current.completion_check_results)? == checks
                && serde_json::to_value(&current.terminal_gaps)? == gaps
                && current.terminal_evidence.as_ref() == Some(&terminal_evidence)
                && current.terminal_reason == Some(terminal_reason);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                FinalizationOutcome::Replayed(current)
            } else {
                FinalizationOutcome::Conflict
            });
        }
        if current.plan_revision != expected_revision || current.wake_epoch != expected_wake_epoch {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let expected_terminal_evidence = terminal_evidence_from_evaluation(
            terminal_evidence.cause.clone(),
            &completion_evaluation,
        )?;
        if terminal_evidence != expected_terminal_evidence {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_run \
             SET status = $3, output = $4, completion_check_results = $5, \
                 terminal_gaps = $6, terminal_cause = $7, \
                 terminal_satisfied_requirement_count = $8, \
                 terminal_requirement_count = $9, terminal_reason = $10, \
                 waiting_reasons = '[]'::JSONB, \
                 wake_epoch = wake_epoch + 1, completed_at = NOW(), updated_at = NOW() \
             WHERE run_uid = $1 AND plan_revision = $2 \
             RETURNING *",
        )
        .bind(run_uid)
        .bind(to_i64(expected_revision, "expected plan revision")?)
        .bind(expected_status.as_str())
        .bind(output)
        .bind(checks)
        .bind(gaps)
        .bind(terminal_cause)
        .bind(to_i64(
            terminal_evidence.satisfied_requirement_count,
            "terminal satisfied requirement count",
        )?)
        .bind(to_i64(
            terminal_evidence.requirement_count,
            "terminal requirement count",
        )?)
        .bind(terminal_reason.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(FinalizationOutcome::Finalized(run))
    }

    /// Atomically cancels one fenced waiting-replan task and finalizes its run.
    pub async fn finalize_replan_stop(
        &self,
        scope: ExecutionScope,
        request: ReplanStopRequest,
    ) -> Result<ReplanStopOutcome> {
        let ReplanStopRequest {
            run_uid,
            expected_revision,
            expected_wake_epoch,
            task_id,
            expected_generation,
            amendment_hash,
            cancellation_reason,
            terminal_projection,
            completion_evaluation,
            terminal_evidence,
            terminal_reason,
        } = request;
        let terminal_status = status_from_completion(completion_evaluation.status);
        if !matches!(
            terminal_status,
            ExecutionRunStatus::Partial | ExecutionRunStatus::Blocked
        ) || status_from_terminal_projection(&terminal_projection) != terminal_status
        {
            return Err(Error::InvalidRepositoryInput {
                message: "replan stop must finalize a matching partial or blocked run".to_string(),
            });
        }
        let selected_reason = execution_terminal_reason(
            &terminal_evidence.cause,
            &terminal_projection,
            &completion_evaluation,
        )?;
        if terminal_reason != selected_reason {
            return Err(Error::InvalidRepositoryInput {
                message: "selected replan-stop terminal reason disagrees with typed evidence"
                    .to_string(),
            });
        }
        let output = terminal_projection_output(&terminal_projection);
        let checks = serde_json::to_value(&completion_evaluation.checks)?;
        let gaps = serde_json::to_value(&completion_evaluation.gaps)?;
        let terminal_cause = serde_json::to_value(&terminal_evidence.cause)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status.is_terminal() {
            let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
                .bind(run_uid)
                .fetch_all(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let tasks = task_rows
                .iter()
                .map(task_from_row)
                .collect::<Result<Vec<_>>>()?;
            let Some(task) = tasks.iter().find(|task| task.task_id == task_id).cloned() else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(ReplanStopOutcome::NotFound);
            };
            let cancelled_outcome = cancelled_task_outcome(&task, &cancellation_reason);
            let amendment_hash_text = amendment_hash.as_ref().map(ToString::to_string);
            let task_ids_to_release = tasks
                .iter()
                .filter(|task| {
                    task.outcome_audit.iter().any(|entry| {
                        entry.get("kind").and_then(Value::as_str) == Some("replan_stopped")
                            && entry.get("accepted").and_then(Value::as_bool) == Some(true)
                            && entry.get("base_plan_revision").and_then(Value::as_u64)
                                == Some(expected_revision)
                            && entry.get("amendment_hash").and_then(Value::as_str)
                                == amendment_hash_text.as_deref()
                    })
                })
                .map(|task| task.task_id)
                .collect::<Vec<_>>();
            let replay = run.plan_revision == expected_revision
                && run.status == terminal_status
                && run.output == output
                && serde_json::to_value(&run.completion_check_results)? == checks
                && serde_json::to_value(&run.terminal_gaps)? == gaps
                && run.terminal_evidence.as_ref() == Some(&terminal_evidence)
                && run.terminal_reason == Some(terminal_reason)
                && task.plan_revision == expected_revision
                && task.generation == expected_generation
                && task.status == ExecutionTaskStatus::Cancelled
                && task.current_outcome.as_ref() == Some(&cancelled_outcome)
                && !task_ids_to_release.is_empty();
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                ReplanStopOutcome::Replayed(Box::new(ReplanStopFinalization {
                    run,
                    task,
                    task_ids_to_release,
                }))
            } else {
                ReplanStopOutcome::Conflict
            });
        }
        let task_rows = sqlx::query(LOAD_NONTERMINAL_TASKS_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::NotFound);
        };
        if run.plan_revision != expected_revision
            || run.wake_epoch != expected_wake_epoch
            || run.status != ExecutionRunStatus::WaitingReplan
            || task.plan_revision != expected_revision
            || task.generation != expected_generation
            || task.status != ExecutionTaskStatus::WaitingReplan
            || !matches!(
                task.current_outcome.as_ref().map(|outcome| &outcome.result),
                Some(ExecutionTaskResult::NeedsReplan { .. })
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::Conflict);
        }
        let expected_terminal_evidence = terminal_evidence_from_evaluation(
            terminal_evidence.cause.clone(),
            &completion_evaluation,
        )?;
        if terminal_evidence != expected_terminal_evidence
            || !matches!(
                terminal_evidence.cause,
                ExecutionTerminalCause::ReplanStop { .. }
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::Conflict);
        }
        let cancellation = terminalize_nonterminal_tasks(
            &mut conn,
            &run,
            &tasks,
            TaskCancellationEvidence {
                kind: "replan_stopped",
                reason: &cancellation_reason,
                base_plan_revision: Some(expected_revision),
                amendment_hash: amendment_hash.as_ref(),
                terminal_status: Some(terminal_status),
                terminal_projection: Some(&terminal_projection),
                completion_evaluation: Some(&completion_evaluation),
            },
        )
        .await?;
        let task_ids_to_release = cancellation
            .tasks
            .iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        let run_row = sqlx::query(FINALIZE_REPLAN_STOP_RUN_SQL)
            .bind(run_uid)
            .bind(to_i64(expected_revision, "expected plan revision")?)
            .bind(terminal_status.as_str())
            .bind(output)
            .bind(checks)
            .bind(gaps)
            .bind(terminal_cause)
            .bind(to_i64(
                terminal_evidence.satisfied_requirement_count,
                "terminal satisfied requirement count",
            )?)
            .bind(to_i64(
                terminal_evidence.requirement_count,
                "terminal requirement count",
            )?)
            .bind(terminal_reason.as_str())
            .bind(to_i64(
                cancellation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.cost_microusd,
                "run consumed cost",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tokens,
                "run consumed tokens",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tool_calls,
                "run consumed tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.retrieved_bytes,
                "run consumed retrieved bytes",
            )?)
            .bind(cancellation.budget_overrun)
            .bind(to_i64(
                tasks.len() as u64,
                "replan-stop cancelled task count",
            )?)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(run_row) = run_row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::Conflict);
        };
        let finalized = ReplanStopFinalization {
            run: run_from_row(&run_row)?,
            task_ids_to_release,
            task: cancellation
                .tasks
                .into_iter()
                .find(|task| task.task_id == task_id)
                .ok_or_else(|| Error::Storage {
                    message: "replan-stop transaction lost its originating task".to_string(),
                })?,
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(ReplanStopOutcome::Finalized(Box::new(finalized)))
    }

    /// Records one cumulative task outcome under the current generation fence.
    ///
    /// Stale, terminal-task, terminal-run, and invalid cumulative messages are
    /// retained in append-only audit history without changing current state.
    pub async fn record_task_outcome(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        outcome: ExecutionTaskOutcome,
    ) -> Result<TaskOutcomeWrite> {
        let validation = moa_artifacts::validation::validate_execution_task_outcome(&outcome);
        if let Some(error) = validation.errors.first() {
            return Err(Error::InvalidRepositoryInput {
                message: format!("{}: {}", error.path, error.message),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::NotFound);
        };
        let task = task_from_row(&task_row)?;

        if task_outcome_is_exact_replay(&task, generation, &outcome) {
            let budget_overrun = run.budget_overrun;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::Replayed {
                run,
                task,
                budget_overrun,
            });
        }

        let rejection = if outcome.schema_version != 1 {
            Some(TaskOutcomeRejection::UnsupportedSchemaVersion)
        } else if run.status.is_terminal() {
            Some(TaskOutcomeRejection::TerminalRun)
        } else if task.status.is_terminal() {
            Some(TaskOutcomeRejection::TerminalTask)
        } else if task.generation != generation {
            Some(TaskOutcomeRejection::StaleGeneration)
        } else if task.status != ExecutionTaskStatus::Running {
            Some(TaskOutcomeRejection::InvalidTaskStatus)
        } else if !usage_is_cumulative(&task.actual, &outcome.usage) {
            Some(TaskOutcomeRejection::NonCumulativeUsage)
        } else {
            None
        };
        if let Some(reason) = rejection {
            let task =
                append_outcome_audit(&mut conn, &task, generation, &outcome, false, Some(reason))
                    .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::Rejected { task, reason });
        }

        let terminal = outcome_is_terminal(&outcome);
        let Some(reconciliation) = reconcile_outcome_usage(&run, &task, &outcome, terminal) else {
            let reason = TaskOutcomeRejection::NonCumulativeUsage;
            let task =
                append_outcome_audit(&mut conn, &task, generation, &outcome, false, Some(reason))
                    .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::Rejected { task, reason });
        };
        let task_status = task_status_for_persisted_outcome(&outcome);
        let run_status = match &outcome.result {
            ExecutionTaskResult::NeedsInput { .. } => ExecutionRunStatus::WaitingInput,
            ExecutionTaskResult::NeedsReplan { .. } => ExecutionRunStatus::WaitingReplan,
            ExecutionTaskResult::Completed { .. }
            | ExecutionTaskResult::Cancelled { .. }
            | ExecutionTaskResult::Failed { .. }
                if matches!(run.status, ExecutionRunStatus::WaitingReview) =>
            {
                ExecutionRunStatus::Running
            }
            ExecutionTaskResult::Completed { .. }
            | ExecutionTaskResult::Cancelled { .. }
            | ExecutionTaskResult::Failed { .. } => run.status,
        };
        let (output, error, citations) = outcome_projection_fields(&outcome)?;
        let audit = outcome_audit_entry(&task, generation, &outcome, true, None);
        let current_outcome = serde_json::to_value(&outcome)?;
        let citations = serde_json::to_value(citations)?;
        let completed_increment = u64::from(task_status == ExecutionTaskStatus::Completed);
        let failed_increment = u64::from(task_status == ExecutionTaskStatus::Failed);
        let cancelled_increment = u64::from(task_status == ExecutionTaskStatus::Cancelled);

        let run_row = sqlx::query(RECONCILE_RUN_OUTCOME_SQL)
            .bind(run_uid)
            .bind(run_status.as_str())
            .bind(to_i64(
                reconciliation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.cost_microusd,
                "run consumed cost",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tokens,
                "run consumed tokens",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tool_calls,
                "run consumed tool calls",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.retrieved_bytes,
                "run consumed retrieved bytes",
            )?)
            .bind(reconciliation.budget_overrun)
            .bind(to_i64(completed_increment, "completed task increment")?)
            .bind(to_i64(failed_increment, "failed task increment")?)
            .bind(to_i64(cancelled_increment, "cancelled task increment")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;

        let row = sqlx::query(RECORD_TASK_OUTCOME_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(to_i64(generation, "task generation")?)
            .bind(task_status.as_str())
            .bind(to_i64(
                reconciliation.remaining_task_reservation.cost_microusd,
                "remaining task cost reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.tokens,
                "remaining task token reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.tasks,
                "remaining task logical reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.tool_calls,
                "remaining task tool-call reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.retrieved_bytes,
                "remaining task byte reservation",
            )?)
            .bind(to_i64(outcome.usage.cost_microusd, "actual task cost")?)
            .bind(to_i64(outcome.usage.tokens, "actual task tokens")?)
            .bind(to_i64(
                u64::from(reconciliation.terminal),
                "actual logical task",
            )?)
            .bind(to_i64(outcome.usage.tool_calls, "actual task tool calls")?)
            .bind(to_i64(
                outcome.usage.retrieved_bytes,
                "actual task retrieved bytes",
            )?)
            .bind(current_outcome)
            .bind(output)
            .bind(error)
            .bind(citations)
            .bind(audit)
            .bind(reconciliation.terminal)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskOutcomeWrite::Applied {
            run,
            task,
            budget_overrun: reconciliation.budget_overrun,
        })
    }

    /// Recovers an exact committed amendment handoff before current-revision validation.
    pub async fn recover_amendment_handoff(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        amendment_hash: &ExecutionHash,
    ) -> Result<AmendmentReplayOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentReplayOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let amendment_hash_text = amendment_hash.to_string();
        let exact_history = run.plan_history.iter().rev().find(|entry| {
            entry.get("base_plan_revision").and_then(Value::as_u64) == Some(expected_revision)
                && entry.get("amendment_hash").and_then(Value::as_str)
                    == Some(amendment_hash_text.as_str())
        });
        let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let audited_task_ids = tasks
            .iter()
            .filter(|task| {
                task.outcome_audit.iter().any(|entry| {
                    entry.get("accepted").and_then(Value::as_bool) == Some(true)
                        && entry.get("amendment_hash").and_then(Value::as_str)
                            == Some(amendment_hash_text.as_str())
                        && entry.get("base_plan_revision").and_then(Value::as_u64)
                            == Some(expected_revision)
                })
            })
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        let task_ids_to_release = match exact_history {
            Some(history) => history
                .get("task_ids_to_release")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<ExecutionTaskId>>(value).ok())
                .filter(|task_ids| !task_ids.is_empty())
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "committed amendment history is missing its handoff task IDs"
                        .to_string(),
                })?,
            None if run.plan_revision == expected_revision && !audited_task_ids.is_empty() => {
                audited_task_ids
            }
            None => {
                let outcome = if run.plan_revision == expected_revision && !run.status.is_terminal()
                {
                    AmendmentReplayOutcome::NotApplied
                } else {
                    AmendmentReplayOutcome::Conflict
                };
                conn.commit().await.map_err(storage_error)?;
                return Ok(outcome);
            }
        };
        let audit_matches = task_ids_to_release.iter().all(|task_id| {
            tasks.iter().any(|task| {
                task.task_id == *task_id
                    && task.outcome_audit.iter().any(|entry| {
                        entry.get("accepted").and_then(Value::as_bool) == Some(true)
                            && entry.get("amendment_hash").and_then(Value::as_str)
                                == Some(amendment_hash_text.as_str())
                            && entry.get("base_plan_revision").and_then(Value::as_u64)
                                == Some(expected_revision)
                    })
            })
        });
        let outcome = if audit_matches {
            AmendmentReplayOutcome::Replayed(Box::new(AmendmentCommit {
                run,
                task_ids_to_release,
            }))
        } else {
            AmendmentReplayOutcome::Conflict
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Appends one compiler-validated amendment under the expected revision fence.
    pub async fn append_amendment(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        validated: ValidatedAmendment,
    ) -> Result<AmendmentWrite> {
        if amendment_hash(&validated.amendment)? != validated.amendment_hash {
            return Err(Error::InvalidRepositoryInput {
                message: "validated amendment hash is inconsistent".to_string(),
            });
        }
        if validated.amendment.base_plan_revision != expected_revision {
            return Ok(AmendmentWrite::Conflict);
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != expected_revision
            || run.status != ExecutionRunStatus::WaitingReplan
            || run.active_plan_hash == validated.active_plan.plan_hash
        {
            let amendment_hash_text = validated.amendment_hash.to_string();
            let exact_history = run.plan_history.iter().rev().any(|entry| {
                entry.get("base_plan_revision").and_then(Value::as_u64) == Some(expected_revision)
                    && entry.get("amendment_hash").and_then(Value::as_str)
                        == Some(amendment_hash_text.as_str())
                    && entry.get("task_ids_to_release")
                        == Some(&json!([validated.superseded_task_id]))
            });
            let exact_task = if exact_history {
                sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
                    .bind(run_uid)
                    .bind(validated.superseded_task_id.as_uuid())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?
                    .map(|row| task_from_row(&row))
                    .transpose()?
                    .is_some_and(|task| {
                        task.outcome_audit.iter().any(|entry| {
                            entry.get("accepted").and_then(Value::as_bool) == Some(true)
                                && entry.get("base_plan_revision").and_then(Value::as_u64)
                                    == Some(expected_revision)
                                && entry.get("amendment_hash").and_then(Value::as_str)
                                    == Some(amendment_hash_text.as_str())
                        })
                    })
            } else {
                false
            };
            conn.commit().await.map_err(storage_error)?;
            return Ok(if exact_task {
                AmendmentWrite::Replayed(Box::new(AmendmentCommit {
                    run,
                    task_ids_to_release: vec![validated.superseded_task_id],
                }))
            } else {
                AmendmentWrite::Conflict
            });
        }
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(validated.superseded_task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::Conflict);
        };
        let task = task_from_row(&task_row)?;
        if task.status != ExecutionTaskStatus::WaitingReplan {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::Conflict);
        }
        let Some(previous_outcome) = task.current_outcome.as_ref() else {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryData {
                message: "waiting-replan task has no persisted outcome".to_string(),
            });
        };
        if !matches!(
            &previous_outcome.result,
            ExecutionTaskResult::NeedsReplan { .. }
        ) {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryData {
                message: "waiting-replan task does not have a needs-replan outcome".to_string(),
            });
        }
        let triggering_failure =
            task_failure_fingerprint_input(&task).ok_or_else(|| Error::InvalidRepositoryData {
                message: "waiting-replan task has no fingerprintable triggering failure"
                    .to_string(),
            })?;
        let triggering_failure_fingerprint = failure_fingerprint(&triggering_failure)?;
        let triggering_failure_fingerprint_text = triggering_failure_fingerprint.to_string();
        let triggering_failure_count = run
            .plan_history
            .iter()
            .filter(|entry| {
                entry.get("failure_fingerprint").and_then(Value::as_str)
                    == Some(triggering_failure_fingerprint_text.as_str())
            })
            .filter_map(|entry| {
                entry
                    .get("failure_fingerprint_count")
                    .and_then(Value::as_u64)
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let Some(run_reserved) = subtract_estimate(run.reserved, task.reserved) else {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryData {
                message: "superseded task reservation exceeds run reservation".to_string(),
            });
        };
        let (consumed_tasks, task_count_overflow) = saturating_db_add(run.consumed.tasks, 1);
        let task_audit = json!({
            "kind": "superseded_by_plan_revision",
            "attempt": task.attempt,
            "generation": task.generation,
            "base_plan_revision": expected_revision,
            "amendment_hash": validated.amendment_hash,
            "accepted": true,
            "recorded_at": Utc::now(),
        });
        let superseded_outcome = serde_json::to_value(ExecutionTaskOutcome {
            schema_version: 1,
            usage: previous_outcome.usage.clone(),
            result: ExecutionTaskResult::Cancelled {
                reason: "superseded_by_plan_revision".to_string(),
            },
        })?;
        let next_revision =
            expected_revision
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepositoryInput {
                    message: "execution plan revision overflow".to_string(),
                })?;
        let history = json!({
            "base_plan_revision": expected_revision,
            "plan_revision": next_revision,
            "amendment": validated.amendment,
            "amendment_hash": validated.amendment_hash,
            "outcome": "applied",
            "task_ids_to_release": [task.task_id],
            "active_plan_hash": validated.active_plan.plan_hash,
            "reason": validated.amendment.reason,
            "requirement_mapping": validated.requirement_mapping,
            "failure_fingerprint": triggering_failure_fingerprint,
            "failure_fingerprint_count": triggering_failure_count,
            "recorded_at": Utc::now(),
        });
        let active_plan = serde_json::to_value(&validated.active_plan)?;
        let prior_run_status = run.status;
        let row = sqlx::query(APPEND_AMENDMENT_SQL)
            .bind(run_uid)
            .bind(to_i64(expected_revision, "expected plan revision")?)
            .bind(to_i64(next_revision, "next plan revision")?)
            .bind(active_plan)
            .bind(validated.active_plan.plan_hash.to_string())
            .bind(history)
            .bind(to_i64(run_reserved.cost_microusd, "run reserved cost")?)
            .bind(to_i64(run_reserved.tokens, "run reserved tokens")?)
            .bind(to_i64(run_reserved.tasks, "run reserved tasks")?)
            .bind(to_i64(run_reserved.tool_calls, "run reserved tool calls")?)
            .bind(to_i64(
                run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(consumed_tasks, "run consumed tasks")?)
            .bind(run.budget_overrun || task_count_overflow)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::Conflict);
        };
        let run = run_from_row(&row)?;
        let superseded_task_row = sqlx::query(SUPERSEDE_REPLAN_TASK_SQL)
            .bind(run_uid)
            .bind(task.task_id.as_uuid())
            .bind(task_audit)
            .bind(superseded_outcome)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let superseded_task = task_from_row(&superseded_task_row)?;
        let metrics = ExecutionMutationMetricEvidence {
            run: run_transition_evidence(prior_run_status, &run),
            tasks: vec![task_transition_evidence(task.status, &superseded_task)],
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(AmendmentWrite::Applied {
            commit: Box::new(AmendmentCommit {
                run,
                task_ids_to_release: vec![task.task_id],
            }),
            metrics: Box::new(metrics),
        })
    }

    /// Atomically cancels a run, all nonterminal work, and all unused reservations.
    pub async fn cancel_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        request: CancellationRequest,
    ) -> Result<CancellationOutcome> {
        let CancellationRequest {
            reason,
            terminal_evidence,
        } = request;
        if terminal_evidence.cause != ExecutionTerminalCause::Cancellation {
            return Err(Error::InvalidRepositoryInput {
                message: "run cancellation requires the cancellation terminal cause".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status == ExecutionRunStatus::Cancelled {
            if run.cancellation_reason.as_deref() != Some(reason.as_str())
                || run.terminal_evidence.as_ref() != Some(&terminal_evidence)
                || run.terminal_reason != Some(ExecutionTerminalReason::Cancelled)
            {
                conn.commit().await.map_err(storage_error)?;
                return Ok(CancellationOutcome::Conflict);
            }
            let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
                .bind(run_uid)
                .fetch_all(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let mut task_ids_to_release = task_rows
                .iter()
                .map(task_from_row)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|task| task_has_accepted_audit_kind(task, "run_cancelled"))
                .map(|task| task.task_id)
                .collect::<Vec<_>>();
            task_ids_to_release.sort();
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::Replayed(Box::new(
                CancellationCommit {
                    run,
                    task_ids_to_release,
                },
            )));
        }
        if run.status.is_terminal() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::Conflict);
        }

        sqlx::query(
            "SELECT task_id FROM moa.execution_task WHERE run_uid = $1 ORDER BY task_id FOR UPDATE",
        )
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let all_tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let expected_terminal_evidence = cancellation_terminal_evidence(
            &run.goal,
            &run.active_plan,
            &scheduling_projection(&run, &all_tasks),
        )?;
        if terminal_evidence != expected_terminal_evidence {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::Conflict);
        }
        let tasks = all_tasks
            .into_iter()
            .filter(|task| !task.status.is_terminal())
            .collect::<Vec<_>>();
        let cancellation = terminalize_nonterminal_tasks(
            &mut conn,
            &run,
            &tasks,
            TaskCancellationEvidence {
                kind: "run_cancelled",
                reason: &reason,
                base_plan_revision: None,
                amendment_hash: None,
                terminal_status: Some(ExecutionRunStatus::Cancelled),
                terminal_projection: None,
                completion_evaluation: None,
            },
        )
        .await?;
        let mut task_ids_to_release = cancellation
            .tasks
            .iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        task_ids_to_release.sort();
        let prior_run_status = run.status;
        let row = sqlx::query(CANCEL_RUN_SQL)
            .bind(run_uid)
            .bind(&reason)
            .bind(serde_json::to_value(&terminal_evidence.cause)?)
            .bind(to_i64(
                terminal_evidence.satisfied_requirement_count,
                "terminal satisfied requirement count",
            )?)
            .bind(to_i64(
                terminal_evidence.requirement_count,
                "terminal requirement count",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.cost_microusd,
                "run consumed cost",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tokens,
                "run consumed tokens",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tool_calls,
                "run consumed tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.retrieved_bytes,
                "run consumed retrieved bytes",
            )?)
            .bind(cancellation.budget_overrun)
            .bind(to_i64(tasks.len() as u64, "cancelled task count")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        let metrics = ExecutionMutationMetricEvidence {
            run: run_transition_evidence(prior_run_status, &run),
            tasks: tasks
                .iter()
                .zip(&cancellation.tasks)
                .map(|(prior, task)| task_transition_evidence(prior.status, task))
                .collect(),
        };
        Ok(CancellationOutcome::Cancelled {
            commit: Box::new(CancellationCommit {
                run,
                task_ids_to_release,
            }),
            metrics: Box::new(metrics),
        })
    }
}

fn run_transition_evidence(
    prior_status: ExecutionRunStatus,
    run: &ExecutionRunRecord,
) -> ExecutionRunTransitionEvidence {
    ExecutionRunTransitionEvidence {
        prior_status,
        status: run.status,
        queued_at: run.queued_at,
        started_at: run.started_at,
        reserved: run.reserved,
        consumed: run.consumed,
        terminal_evidence: run.terminal_evidence.clone(),
        terminal_reason: run.terminal_reason,
    }
}

fn task_transition_evidence(
    prior_status: ExecutionTaskStatus,
    task: &ExecutionTaskRecord,
) -> ExecutionTaskTransitionEvidence {
    ExecutionTaskTransitionEvidence {
        prior_status,
        status: task.status,
        kind: task.kind.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        started_at: task.started_at,
        completed_at: task.completed_at,
    }
}

fn task_outcome_is_exact_replay(
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
) -> bool {
    task.current_outcome.as_ref() == Some(outcome)
        && task.outcome_audit.iter().any(|entry| {
            entry.get("received_generation").and_then(Value::as_u64) == Some(generation)
                && entry.get("accepted").and_then(Value::as_bool) == Some(true)
                && entry
                    .get("outcome")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ExecutionTaskOutcome>(value).ok())
                    .as_ref()
                    == Some(outcome)
        })
}

fn task_has_accepted_audit_kind(task: &ExecutionTaskRecord, kind: &str) -> bool {
    task.outcome_audit.iter().any(|entry| {
        entry.get("kind").and_then(Value::as_str) == Some(kind)
            && entry.get("accepted").and_then(Value::as_bool) == Some(true)
    })
}

#[derive(Clone, Copy)]
struct TaskCancellationEvidence<'a> {
    kind: &'a str,
    reason: &'a str,
    base_plan_revision: Option<u64>,
    amendment_hash: Option<&'a ExecutionHash>,
    terminal_status: Option<ExecutionRunStatus>,
    terminal_projection: Option<&'a TerminalProjection>,
    completion_evaluation: Option<&'a CompletionEvaluation>,
}

struct TaskCancellationWrite {
    tasks: Vec<ExecutionTaskRecord>,
    run_reserved: ExecutionEstimate,
    run_consumed: ExecutionEstimate,
    budget_overrun: bool,
}

async fn terminalize_nonterminal_tasks(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    tasks: &[ExecutionTaskRecord],
    evidence: TaskCancellationEvidence<'_>,
) -> Result<TaskCancellationWrite> {
    let mut ledger = run.clone();
    let mut terminalized = Vec::with_capacity(tasks.len());
    for task in tasks {
        let outcome = cancelled_task_outcome(task, evidence.reason);
        let reconciliation =
            reconcile_outcome_usage(&ledger, task, &outcome, true).ok_or_else(|| {
                Error::InvalidRepositoryData {
                    message: format!(
                        "{} cancellation could not reconcile task {}",
                        evidence.kind, task.task_id
                    ),
                }
            })?;
        let audit = json!({
            "kind": evidence.kind,
            "attempt": task.attempt,
            "generation": task.generation,
            "plan_revision": task.plan_revision,
            "base_plan_revision": evidence.base_plan_revision,
            "amendment_hash": evidence.amendment_hash,
            "accepted": true,
            "reason": evidence.reason,
            "terminal_status": evidence.terminal_status.map(ExecutionRunStatus::as_str),
            "terminal_projection": evidence.terminal_projection,
            "completion_evaluation": evidence.completion_evaluation,
            "outcome": &outcome,
            "recorded_at": Utc::now(),
        });
        let (_, error, citations) = outcome_projection_fields(&outcome)?;
        let row = sqlx::query(TERMINALIZE_CANCELLED_TASK_SQL)
            .bind(task.run_uid)
            .bind(task.task_id.as_uuid())
            .bind(to_i64(task.generation, "expected task generation")?)
            .bind(serde_json::to_value(&outcome)?)
            .bind(error)
            .bind(serde_json::to_value(citations)?)
            .bind(audit)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::Storage {
                message: format!(
                    "{} cancellation lost locked task {}",
                    evidence.kind, task.task_id
                ),
            })?;
        terminalized.push(task_from_row(&row)?);
        ledger.reserved = reconciliation.run_reserved;
        ledger.consumed = reconciliation.run_consumed;
        ledger.budget_overrun = reconciliation.budget_overrun;
    }
    Ok(TaskCancellationWrite {
        tasks: terminalized,
        run_reserved: ledger.reserved,
        run_consumed: ledger.consumed,
        budget_overrun: ledger.budget_overrun,
    })
}

fn cancelled_task_outcome(task: &ExecutionTaskRecord, reason: &str) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: task.actual.clone(),
        result: ExecutionTaskResult::Cancelled {
            reason: reason.to_string(),
        },
    }
}

fn budget_ledger(run: &ExecutionRunRecord) -> BudgetLedger {
    BudgetLedger {
        limit: run.approved_budget.clone(),
        reserved: run.reserved,
        consumed: run.consumed,
        overrun: run.budget_overrun,
    }
}

const fn status_from_completion(status: CompletionStatus) -> ExecutionRunStatus {
    match status {
        CompletionStatus::Completed => ExecutionRunStatus::Completed,
        CompletionStatus::Partial => ExecutionRunStatus::Partial,
        CompletionStatus::Blocked => ExecutionRunStatus::Blocked,
        CompletionStatus::Unsupported => ExecutionRunStatus::Unsupported,
        CompletionStatus::Failed => ExecutionRunStatus::Failed,
    }
}

const fn status_from_terminal_projection(projection: &TerminalProjection) -> ExecutionRunStatus {
    match projection {
        TerminalProjection::Completed { .. } => ExecutionRunStatus::Completed,
        TerminalProjection::Partial { .. } => ExecutionRunStatus::Partial,
        TerminalProjection::Blocked { .. } => ExecutionRunStatus::Blocked,
        TerminalProjection::Unsupported { .. } => ExecutionRunStatus::Unsupported,
        TerminalProjection::Failed { .. } => ExecutionRunStatus::Failed,
        TerminalProjection::Cancelled { .. } => ExecutionRunStatus::Cancelled,
    }
}

fn terminal_projection_output(projection: &TerminalProjection) -> Option<Value> {
    match projection {
        TerminalProjection::Completed { output } => Some(output.clone()),
        TerminalProjection::Partial { output, .. } | TerminalProjection::Blocked { output, .. } => {
            output.clone()
        }
        TerminalProjection::Unsupported { .. }
        | TerminalProjection::Failed { .. }
        | TerminalProjection::Cancelled { .. } => None,
    }
}

fn scheduling_projection(
    run: &ExecutionRunRecord,
    tasks: &[ExecutionTaskRecord],
) -> ExecutionProjection {
    let task_projections = tasks
        .iter()
        .map(|task| ExecutionTaskProjection {
            task_id: task.task_id,
            node_id: task.node_id.clone(),
            item_key: task.item_key.clone(),
            status: task.status,
            attempt: task.attempt,
            generation: task.generation,
            input: task.input.clone(),
            outcome: task.current_outcome.clone(),
        })
        .collect::<Vec<_>>();
    let mut node_statuses = BTreeMap::new();
    for node in &run.active_plan.definition.nodes {
        let node_tasks = tasks
            .iter()
            .filter(|task| task.node_id == node.id)
            .collect::<Vec<_>>();
        let status = persisted_node_status(&node.operation, &node_tasks);
        node_statuses.insert(node.id.clone(), status);
    }
    ExecutionProjection {
        plan_revision: run.plan_revision,
        node_statuses,
        tasks: task_projections,
    }
}

fn persisted_node_status(
    operation: &ExecutionOperation,
    tasks: &[&ExecutionTaskRecord],
) -> ExecutionNodeStatus {
    if tasks.is_empty() {
        return ExecutionNodeStatus::Pending;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::WaitingInput | ExecutionTaskStatus::WaitingReplan
        ) || (task.status == ExecutionTaskStatus::Running
            && matches!(
                task.kind,
                LogicalTaskKind::Review { .. } | LogicalTaskKind::WaitSignal { .. }
            ))
    }) {
        return ExecutionNodeStatus::Waiting;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::Pending
                | ExecutionTaskStatus::Reserved
                | ExecutionTaskStatus::Running
        )
    }) {
        return ExecutionNodeStatus::Running;
    }
    if tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::Failed)
    {
        return ExecutionNodeStatus::Failed;
    }
    if tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::Cancelled)
    {
        return ExecutionNodeStatus::Cancelled;
    }
    if matches!(
        operation,
        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. }
    ) {
        return ExecutionNodeStatus::Pending;
    }
    if tasks
        .iter()
        .all(|task| task.status == ExecutionTaskStatus::Skipped)
    {
        ExecutionNodeStatus::Skipped
    } else {
        ExecutionNodeStatus::Completed
    }
}

fn usage_is_cumulative(previous: &ExecutionUsage, cumulative: &ExecutionUsage) -> bool {
    cumulative.cost_microusd >= previous.cost_microusd
        && cumulative.tokens >= previous.tokens
        && cumulative.tool_calls >= previous.tool_calls
        && cumulative.retrieved_bytes >= previous.retrieved_bytes
}

fn task_failure_fingerprint_input(task: &ExecutionTaskRecord) -> Option<FailureFingerprintInput> {
    let outcome = task.current_outcome.as_ref()?;
    let (class, message) = match &outcome.result {
        ExecutionTaskResult::Failed { class, message } => (class.clone(), message.clone()),
        ExecutionTaskResult::NeedsReplan { reason, .. } => (
            moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
            reason.clone(),
        ),
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

fn resume_budget_exhausted(run: &ExecutionRunRecord, task: &ExecutionTaskRecord) -> bool {
    run.budget_overrun
        || resource_dimension_exhausted(
            run.approved_budget.max_cost_microusd,
            run.consumed.cost_microusd,
            run.reserved.cost_microusd,
            task.estimate.cost_microusd,
            task.reserved.cost_microusd,
        )
        || resource_dimension_exhausted(
            run.approved_budget.max_tokens,
            run.consumed.tokens,
            run.reserved.tokens,
            task.estimate.tokens,
            task.reserved.tokens,
        )
        || resource_dimension_exhausted(
            run.approved_budget.max_tool_calls,
            run.consumed.tool_calls,
            run.reserved.tool_calls,
            task.estimate.tool_calls,
            task.reserved.tool_calls,
        )
        || resource_dimension_exhausted(
            run.approved_budget.max_retrieved_bytes,
            run.consumed.retrieved_bytes,
            run.reserved.retrieved_bytes,
            task.estimate.retrieved_bytes,
            task.reserved.retrieved_bytes,
        )
}

fn resource_dimension_exhausted(
    limit: Option<u64>,
    consumed: u64,
    reserved: u64,
    task_estimate: u64,
    task_reserved: u64,
) -> bool {
    task_estimate > 0
        && task_reserved == 0
        && limit.is_some_and(|limit| consumed.saturating_add(reserved) >= limit)
}

fn outcome_is_terminal(outcome: &ExecutionTaskOutcome) -> bool {
    !matches!(
        &outcome.result,
        ExecutionTaskResult::NeedsInput { .. }
            | ExecutionTaskResult::NeedsReplan { .. }
            | ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
                ..
            }
    )
}

fn task_status_for_persisted_outcome(outcome: &ExecutionTaskOutcome) -> ExecutionTaskStatus {
    match &outcome.result {
        ExecutionTaskResult::Completed { .. } => ExecutionTaskStatus::Completed,
        ExecutionTaskResult::NeedsInput { .. } => ExecutionTaskStatus::WaitingInput,
        ExecutionTaskResult::NeedsReplan { .. } => ExecutionTaskStatus::WaitingReplan,
        ExecutionTaskResult::Cancelled { .. } => ExecutionTaskStatus::Cancelled,
        ExecutionTaskResult::Failed {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
            ..
        } => ExecutionTaskStatus::Running,
        ExecutionTaskResult::Failed { .. } => ExecutionTaskStatus::Failed,
    }
}

fn outcome_projection_fields(
    outcome: &ExecutionTaskOutcome,
) -> Result<(Option<Value>, Option<Value>, Vec<ExecutionCitation>)> {
    match &outcome.result {
        ExecutionTaskResult::Completed { output, citations } => {
            Ok((Some(output.clone()), None, citations.clone()))
        }
        ExecutionTaskResult::NeedsInput { question, audience } => Ok((
            None,
            Some(json!({ "question": question, "audience": audience })),
            Vec::new(),
        )),
        ExecutionTaskResult::NeedsReplan { reason, evidence } => Ok((
            None,
            Some(json!({ "reason": reason, "evidence": evidence })),
            Vec::new(),
        )),
        ExecutionTaskResult::Cancelled { reason } => Ok((
            None,
            Some(json!({ "class": "cancelled", "message": reason })),
            Vec::new(),
        )),
        ExecutionTaskResult::Failed { class, message } => Ok((
            None,
            Some(json!({ "class": class, "message": message })),
            Vec::new(),
        )),
    }
}

fn outcome_audit_entry(
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
    accepted: bool,
    rejection: Option<TaskOutcomeRejection>,
) -> Value {
    json!({
        "received_attempt": attempt_for_generation(task, generation),
        "received_generation": generation,
        "accepted": accepted,
        "rejection": rejection,
        "outcome": outcome,
        "recorded_at": Utc::now(),
    })
}

fn attempt_for_generation(task: &ExecutionTaskRecord, generation: u64) -> Option<u64> {
    task.generation_history.iter().find_map(|entry| {
        (entry.get("generation").and_then(Value::as_u64) == Some(generation))
            .then(|| entry.get("attempt").and_then(Value::as_u64))
            .flatten()
    })
}

async fn append_outcome_audit(
    conn: &mut ScopedConn<'_>,
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
    accepted: bool,
    rejection: Option<TaskOutcomeRejection>,
) -> Result<ExecutionTaskRecord> {
    let audit = outcome_audit_entry(task, generation, outcome, accepted, rejection);
    let row = sqlx::query(APPEND_TASK_OUTCOME_AUDIT_SQL)
        .bind(task.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(audit)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    task_from_row(&row)
}

fn terminal_reservation_rejection(task: &ExecutionTaskRecord) -> Option<ReservationRejection> {
    if task.status != ExecutionTaskStatus::Failed {
        return None;
    }
    let audit = task.outcome_audit.last()?;
    if audit.get("kind").and_then(Value::as_str) != Some("reservation_admission_rejected")
        || audit.get("generation").and_then(Value::as_u64) != Some(task.generation)
    {
        return None;
    }
    match audit.get("rejection").and_then(Value::as_str) {
        Some("DeadlineElapsed") => Some(ReservationRejection::DeadlineElapsed),
        Some("BudgetExceeded") => Some(ReservationRejection::BudgetExceeded),
        _ => None,
    }
}

async fn terminalize_reservation_rejection(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    rejection: ReservationRejection,
) -> Result<ReservationTerminalization> {
    let class = match rejection {
        ReservationRejection::DeadlineElapsed => {
            moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded
        }
        ReservationRejection::BudgetExceeded => {
            moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded
        }
        _ => {
            return Err(Error::InvalidRepositoryInput {
                message: "only deadline or budget admission failures may terminalize a task"
                    .to_string(),
            });
        }
    };
    let outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: task.actual.clone(),
        result: ExecutionTaskResult::Failed {
            class,
            message: format!("execution task reservation rejected: {rejection:?}"),
        },
    };
    // The database state machine requires failed tasks to pass through reserved and running.
    // These transitions stay inside this transaction and intentionally set no dispatch
    // timestamps or resource reservations because admission rejected the work before dispatch.
    let reserved = sqlx::query(
        "UPDATE moa.execution_task SET status = 'reserved', updated_at = NOW() \
         WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'pending'",
    )
    .bind(task.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(task.generation, "task generation")?)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if reserved.rows_affected() != 1 {
        return Err(Error::Storage {
            message: "reservation terminalization lost its locked pending task".to_string(),
        });
    }
    let running = sqlx::query(
        "UPDATE moa.execution_task SET status = 'running', updated_at = NOW() \
         WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'reserved'",
    )
    .bind(task.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(task.generation, "task generation")?)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if running.rows_affected() != 1 {
        return Err(Error::Storage {
            message: "reservation terminalization lost its locked reserved task".to_string(),
        });
    }
    sqlx::query(RECONCILE_RUN_OUTCOME_SQL)
        .bind(run.run_uid)
        .bind(run.status.as_str())
        .bind(to_i64(run.reserved.cost_microusd, "run reserved cost")?)
        .bind(to_i64(run.reserved.tokens, "run reserved tokens")?)
        .bind(to_i64(run.reserved.tasks, "run reserved tasks")?)
        .bind(to_i64(run.reserved.tool_calls, "run reserved tool calls")?)
        .bind(to_i64(
            run.reserved.retrieved_bytes,
            "run reserved retrieved bytes",
        )?)
        .bind(to_i64(run.consumed.cost_microusd, "run consumed cost")?)
        .bind(to_i64(run.consumed.tokens, "run consumed tokens")?)
        .bind(to_i64(run.consumed.tasks, "run consumed tasks")?)
        .bind(to_i64(run.consumed.tool_calls, "run consumed tool calls")?)
        .bind(to_i64(
            run.consumed.retrieved_bytes,
            "run consumed retrieved bytes",
        )?)
        .bind(run.budget_overrun)
        .bind(0_i64)
        .bind(1_i64)
        .bind(0_i64)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;

    let (_, error, citations) = outcome_projection_fields(&outcome)?;
    let audit = json!({
        "kind": "reservation_admission_rejected",
        "attempt": task.attempt,
        "generation": task.generation,
        "accepted": true,
        "rejection": format!("{rejection:?}"),
        "outcome": &outcome,
        "recorded_at": Utc::now(),
    });
    let task_row = sqlx::query(RECORD_RESERVATION_REJECTION_SQL)
        .bind(task.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(to_i64(task.generation, "task generation")?)
        .bind(to_i64(task.actual.cost_microusd, "actual task cost")?)
        .bind(to_i64(task.actual.tokens, "actual task tokens")?)
        .bind(to_i64(task.actual.tool_calls, "actual task tool calls")?)
        .bind(to_i64(
            task.actual.retrieved_bytes,
            "actual task retrieved bytes",
        )?)
        .bind(serde_json::to_value(&outcome)?)
        .bind(error)
        .bind(serde_json::to_value(citations)?)
        .bind(audit)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let Some(task_row) = task_row else {
        return Err(Error::Storage {
            message: "reservation terminalization lost its locked generation fence".to_string(),
        });
    };
    let run_row = sqlx::query(LOAD_RUN_SQL)
        .bind(run.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    Ok(ReservationTerminalization {
        run: run_from_row(&run_row)?,
        task: task_from_row(&task_row)?,
        rejection,
    })
}

fn redispatch_is_exact_replay(
    task: &ExecutionTaskRecord,
    history_kind: &str,
    requested_generation: u64,
    resume_input: Option<&Value>,
) -> bool {
    let Some(history) = task.generation_history.last() else {
        return false;
    };
    let matches_generation = history.get("kind").and_then(Value::as_str) == Some(history_kind)
        && history.get("requested_generation").and_then(Value::as_u64)
            == Some(requested_generation)
        && history.get("generation").and_then(Value::as_u64) == Some(task.generation);
    if !matches_generation {
        return false;
    }
    history_kind != "input_resume" || task.resume_input_history.last() == resume_input
}

async fn terminalize_redispatch_rejection(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    history_kind: &str,
    class: moa_artifacts::execution_plan::ExecutionFailureClass,
    reason: TransitionRejection,
) -> Result<ExecutionTaskRecord> {
    let outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: task.actual.clone(),
        result: ExecutionTaskResult::Failed {
            class,
            message: format!("execution task {history_kind} rejected: {reason:?}"),
        },
    };
    let reconciliation = reconcile_outcome_usage(run, task, &outcome, true).ok_or_else(|| {
        Error::InvalidRepositoryData {
            message: "redispatch rejection could not reconcile cumulative usage".to_string(),
        }
    })?;
    sqlx::query(RECONCILE_RUN_OUTCOME_SQL)
        .bind(run.run_uid)
        .bind(ExecutionRunStatus::Running.as_str())
        .bind(to_i64(
            reconciliation.run_reserved.cost_microusd,
            "run reserved cost",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.tokens,
            "run reserved tokens",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.tasks,
            "run reserved tasks",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.tool_calls,
            "run reserved tool calls",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.retrieved_bytes,
            "run reserved retrieved bytes",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.cost_microusd,
            "run consumed cost",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.tokens,
            "run consumed tokens",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.tasks,
            "run consumed tasks",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.tool_calls,
            "run consumed tool calls",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.retrieved_bytes,
            "run consumed retrieved bytes",
        )?)
        .bind(reconciliation.budget_overrun)
        .bind(0_i64)
        .bind(1_i64)
        .bind(0_i64)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;

    let (_, error, citations) = outcome_projection_fields(&outcome)?;
    let audit = outcome_audit_entry(task, task.generation, &outcome, true, None);
    let row = sqlx::query(RECORD_TASK_OUTCOME_SQL)
        .bind(task.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(to_i64(task.generation, "task generation")?)
        .bind(ExecutionTaskStatus::Failed.as_str())
        .bind(to_i64(
            reconciliation.remaining_task_reservation.cost_microusd,
            "remaining task cost reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.tokens,
            "remaining task token reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.tasks,
            "remaining task logical reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.tool_calls,
            "remaining task tool-call reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.retrieved_bytes,
            "remaining task byte reservation",
        )?)
        .bind(to_i64(task.actual.cost_microusd, "actual task cost")?)
        .bind(to_i64(task.actual.tokens, "actual task tokens")?)
        .bind(1_i64)
        .bind(to_i64(task.actual.tool_calls, "actual task tool calls")?)
        .bind(to_i64(
            task.actual.retrieved_bytes,
            "actual task retrieved bytes",
        )?)
        .bind(serde_json::to_value(&outcome)?)
        .bind(Option::<Value>::None)
        .bind(error)
        .bind(serde_json::to_value(citations)?)
        .bind(audit)
        .bind(true)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    task_from_row(&row)
}

impl ExecutionRepository {
    /// Confirms the exact displayed active-plan hash and atomically persists its budget.
    pub async fn confirm_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_plan_hash: &ExecutionHash,
        approved_budget: ExecutionBudgetLimit,
    ) -> Result<ConfirmationOutcome> {
        let budget = DbBudgetLimit::try_from(&approved_budget)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ConfirmationOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.active_plan_hash != *expected_plan_hash {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ConfirmationOutcome::Conflict(
                ConfirmationConflict::PlanHashMismatch,
            ));
        }
        if current.status != ExecutionRunStatus::AwaitingConfirmation {
            let outcome = if current.status.is_terminal()
                || current.confirmed_at.is_none()
                || current.confirmed_plan_hash.as_ref() != Some(expected_plan_hash)
            {
                ConfirmationOutcome::Conflict(ConfirmationConflict::InvalidStatus)
            } else if current.approved_budget == approved_budget {
                ConfirmationOutcome::AlreadyConfirmed(current)
            } else {
                ConfirmationOutcome::Conflict(ConfirmationConflict::BudgetMismatch)
            };
            conn.commit().await.map_err(storage_error)?;
            return Ok(outcome);
        }

        let row = sqlx::query(CONFIRM_RUN_SQL)
            .bind(run_uid)
            .bind(expected_plan_hash.to_string())
            .bind(budget.max_cost_microusd)
            .bind(budget.max_tokens)
            .bind(budget.max_tasks)
            .bind(budget.max_tool_calls)
            .bind(budget.max_retrieved_bytes)
            .bind(budget.deadline_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ConfirmationOutcome::Conflict(
                ConfirmationConflict::InvalidStatus,
            ));
        };
        let confirmed = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ConfirmationOutcome::Confirmed(confirmed))
    }

    /// Materializes stable logical tasks exactly once for the active plan revision.
    pub async fn materialize_tasks(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        plan_revision: u64,
        tasks: Vec<LogicalTask>,
    ) -> Result<Vec<ExecutionTaskRecord>> {
        match self
            .materialize_node(scope, run_uid, plan_revision, None, tasks)
            .await?
        {
            MaterializationOutcome::Applied(evidence) => Ok(evidence.tasks),
            MaterializationOutcome::Replayed { tasks } => Ok(tasks),
            MaterializationOutcome::Conflict => Err(Error::InvalidRepositoryInput {
                message: "task materialization conflicts with first persisted semantics"
                    .to_string(),
            }),
        }
    }

    /// Materializes one node and returns first-application evidence for metric emission.
    pub async fn materialize_node(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        plan_revision: u64,
        marker: Option<ExecutionNodeMaterialization>,
        tasks: Vec<LogicalTask>,
    ) -> Result<MaterializationOutcome> {
        let plan_revision_db = to_i64(plan_revision, "plan revision")?;
        if let Some(marker) = marker.as_ref()
            && tasks.iter().any(|task| task.node_id != marker.node_id())
        {
            return Err(Error::InvalidRepositoryInput {
                message: "aggregate materialization tasks must share the marker node".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message: "cannot materialize tasks for a missing run".to_string(),
            });
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != plan_revision {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message: "task materialization plan revision is stale".to_string(),
            });
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message: "tasks may materialize only for queued or running runs".to_string(),
            });
        }

        let marker_applied = if let Some(marker) = marker.as_ref() {
            let inserted = sqlx::query(INSERT_NODE_MATERIALIZATION_SQL)
                .bind(run_uid)
                .bind(run.tenant_id.0)
                .bind(run.contact_id.map(|value| value.0))
                .bind(plan_revision_db)
                .bind(marker.node_id())
                .bind(marker.kind_label())
                .bind(
                    marker
                        .fanout_items()
                        .map(|value| to_i64(value, "map fanout items"))
                        .transpose()?,
                )
                .bind(
                    marker
                        .reducer_depth()
                        .map(|value| to_i64(value, "reducer depth"))
                        .transpose()?,
                )
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            if inserted.is_none() {
                let existing = sqlx::query(LOAD_NODE_MATERIALIZATION_SQL)
                    .bind(run_uid)
                    .bind(plan_revision_db)
                    .bind(marker.node_id())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                let Some(existing) = existing else {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(MaterializationOutcome::Conflict);
                };
                let existing_kind: String = existing.try_get("kind").map_err(row_error)?;
                let existing_fanout = optional_u64(&existing, "fanout_items")?;
                let existing_depth = optional_u64(&existing, "reducer_depth")?;
                if existing_kind != marker.kind_label()
                    || existing_fanout != marker.fanout_items()
                    || existing_depth != marker.reducer_depth()
                {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(MaterializationOutcome::Conflict);
                }
                false
            } else {
                true
            }
        } else {
            false
        };

        let mut records = Vec::with_capacity(tasks.len());
        let mut inserted_task_ids = Vec::new();
        let mut inserted_count = 0_u64;
        for task in tasks {
            validate_logical_task(run_uid, plan_revision, &task)?;
            let requirement_ids = serde_json::to_value(&task.requirement_ids)?;
            let kind = serde_json::to_value(&task.kind)?;
            let retry = serde_json::to_value(&task.retry)?;
            let generation = to_i64(task.generation, "task generation")?;
            let estimate = DbEstimate::try_from(task.reservation)?;
            let generation_history = json!([{
                "kind": "initial",
                "attempt": 1,
                "generation": task.generation,
            }]);
            let inserted = sqlx::query(INSERT_TASK_SQL)
                .bind(task.task_id.as_uuid())
                .bind(run_uid)
                .bind(run.tenant_id.0)
                .bind(run.contact_id.map(|value| value.0))
                .bind(&task.node_id)
                .bind(&task.item_key)
                .bind(requirement_ids)
                .bind(plan_revision_db)
                .bind(generation)
                .bind(&task.input)
                .bind(kind)
                .bind(retry)
                .bind(estimate.cost_microusd)
                .bind(estimate.tokens)
                .bind(estimate.tasks)
                .bind(estimate.tool_calls)
                .bind(estimate.retrieved_bytes)
                .bind(generation_history)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;

            let record = if let Some(row) = inserted {
                inserted_count = inserted_count.saturating_add(1);
                let task = task_from_row(&row)?;
                inserted_task_ids.push(task.task_id);
                task
            } else {
                let row = sqlx::query(LOAD_TASK_BY_LOGICAL_KEY_SQL)
                    .bind(run_uid)
                    .bind(&task.node_id)
                    .bind(&task.item_key)
                    .fetch_one(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                let existing = task_from_row(&row)?;
                ensure_materialization_replay_matches(&existing, &task)?;
                existing
            };
            records.push(record);
        }
        inserted_task_ids.sort();

        if inserted_count > 0 || marker_applied {
            sqlx::query(
                "UPDATE moa.execution_run \
                 SET progress_total_tasks = progress_total_tasks + $2, \
                     wake_epoch = wake_epoch + 1, updated_at = NOW() \
                 WHERE run_uid = $1",
            )
            .bind(run_uid)
            .bind(to_i64(inserted_count, "inserted task count")?)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        }
        conn.commit().await.map_err(storage_error)?;
        if inserted_count > 0 || marker_applied {
            Ok(MaterializationOutcome::Applied(MaterializationEvidence {
                tasks: records,
                inserted_task_ids,
                marker,
            }))
        } else {
            Ok(MaterializationOutcome::Replayed { tasks: records })
        }
    }
}

fn validate_logical_task(run_uid: Uuid, plan_revision: u64, task: &LogicalTask) -> Result<()> {
    if task.plan_revision != plan_revision {
        return Err(Error::InvalidRepositoryInput {
            message: format!("task `{}` has the wrong plan revision", task.task_id),
        });
    }
    if task.generation != 1 {
        return Err(Error::InvalidRepositoryInput {
            message: format!("new task `{}` must start at generation one", task.task_id),
        });
    }
    if task.reservation.tasks != 1 {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "task `{}` must reserve exactly one logical task",
                task.task_id
            ),
        });
    }
    let expected = ExecutionTaskId::derive(run_uid, &task.node_id, &task.item_key)?;
    if expected != task.task_id {
        return Err(Error::InvalidRepositoryInput {
            message: format!("task `{}` does not match its stable identity", task.task_id),
        });
    }
    Ok(())
}

fn ensure_materialization_replay_matches(
    existing: &ExecutionTaskRecord,
    requested: &LogicalTask,
) -> Result<()> {
    if existing.task_id != requested.task_id
        || existing.requirement_ids != requested.requirement_ids
        || existing.plan_revision != requested.plan_revision
        || existing.input != requested.input
        || existing.kind != requested.kind
        || existing.retry != requested.retry
        || existing.estimate != requested.reservation
    {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "logical task identity ({}, {}, {}) was replayed with different semantics",
                existing.run_uid, existing.node_id, existing.item_key
            ),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct DbEstimate {
    cost_microusd: i64,
    tokens: i64,
    tasks: i64,
    tool_calls: i64,
    retrieved_bytes: i64,
}

impl TryFrom<ExecutionEstimate> for DbEstimate {
    type Error = Error;

    fn try_from(value: ExecutionEstimate) -> Result<Self> {
        Ok(Self {
            cost_microusd: to_i64(value.cost_microusd, "task estimated cost")?,
            tokens: to_i64(value.tokens, "task estimated tokens")?,
            tasks: to_i64(value.tasks, "task estimated logical tasks")?,
            tool_calls: to_i64(value.tool_calls, "task estimated tool calls")?,
            retrieved_bytes: to_i64(value.retrieved_bytes, "task estimated retrieved bytes")?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Reconciliation {
    remaining_task_reservation: ExecutionEstimate,
    run_reserved: ExecutionEstimate,
    run_consumed: ExecutionEstimate,
    budget_overrun: bool,
    terminal: bool,
}

fn reconcile_outcome_usage(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    outcome: &ExecutionTaskOutcome,
    terminal: bool,
) -> Option<Reconciliation> {
    let delta = ExecutionEstimate {
        cost_microusd: outcome
            .usage
            .cost_microusd
            .checked_sub(task.actual.cost_microusd)?,
        tokens: outcome.usage.tokens.checked_sub(task.actual.tokens)?,
        tool_calls: outcome
            .usage
            .tool_calls
            .checked_sub(task.actual.tool_calls)?,
        retrieved_bytes: outcome
            .usage
            .retrieved_bytes
            .checked_sub(task.actual.retrieved_bytes)?,
        tasks: 0,
    };
    let usage_overrun = delta.cost_microusd > task.reserved.cost_microusd
        || delta.tokens > task.reserved.tokens
        || delta.tool_calls > task.reserved.tool_calls
        || delta.retrieved_bytes > task.reserved.retrieved_bytes;
    let release = if terminal {
        task.reserved
    } else {
        ExecutionEstimate {
            cost_microusd: delta.cost_microusd.min(task.reserved.cost_microusd),
            tokens: delta.tokens.min(task.reserved.tokens),
            tasks: 0,
            tool_calls: delta.tool_calls.min(task.reserved.tool_calls),
            retrieved_bytes: delta.retrieved_bytes.min(task.reserved.retrieved_bytes),
        }
    };
    let remaining_task_reservation = subtract_estimate(task.reserved, release)?;
    let run_reserved = subtract_estimate(run.reserved, release)?;
    let (cost_microusd, cost_overflow) =
        saturating_db_add(run.consumed.cost_microusd, delta.cost_microusd);
    let (tokens, token_overflow) = saturating_db_add(run.consumed.tokens, delta.tokens);
    let (tool_calls, tool_overflow) = saturating_db_add(run.consumed.tool_calls, delta.tool_calls);
    let (retrieved_bytes, bytes_overflow) =
        saturating_db_add(run.consumed.retrieved_bytes, delta.retrieved_bytes);
    let (tasks, task_overflow) = saturating_db_add(run.consumed.tasks, u64::from(terminal));
    let run_consumed = ExecutionEstimate {
        cost_microusd,
        tokens,
        tasks,
        tool_calls,
        retrieved_bytes,
    };
    let limit_overrun = exceeds_limit(run_consumed, run_reserved, &run.approved_budget);
    Some(Reconciliation {
        remaining_task_reservation,
        run_reserved,
        run_consumed,
        budget_overrun: run.budget_overrun
            || usage_overrun
            || cost_overflow
            || token_overflow
            || tool_overflow
            || bytes_overflow
            || task_overflow
            || limit_overrun,
        terminal,
    })
}

fn subtract_estimate(
    left: ExecutionEstimate,
    right: ExecutionEstimate,
) -> Option<ExecutionEstimate> {
    Some(ExecutionEstimate {
        cost_microusd: left.cost_microusd.checked_sub(right.cost_microusd)?,
        tokens: left.tokens.checked_sub(right.tokens)?,
        tasks: left.tasks.checked_sub(right.tasks)?,
        tool_calls: left.tool_calls.checked_sub(right.tool_calls)?,
        retrieved_bytes: left.retrieved_bytes.checked_sub(right.retrieved_bytes)?,
    })
}

fn saturating_db_add(left: u64, right: u64) -> (u64, bool) {
    let sum = u128::from(left) + u128::from(right);
    let maximum = i64::MAX as u128;
    (sum.min(maximum) as u64, sum > maximum)
}

fn exceeds_limit(
    consumed: ExecutionEstimate,
    reserved: ExecutionEstimate,
    limit: &ExecutionBudgetLimit,
) -> bool {
    exceeds_optional(
        consumed.cost_microusd,
        reserved.cost_microusd,
        limit.max_cost_microusd,
    ) || exceeds_optional(consumed.tokens, reserved.tokens, limit.max_tokens)
        || exceeds_optional(consumed.tasks, reserved.tasks, limit.max_tasks)
        || exceeds_optional(
            consumed.tool_calls,
            reserved.tool_calls,
            limit.max_tool_calls,
        )
        || exceeds_optional(
            consumed.retrieved_bytes,
            reserved.retrieved_bytes,
            limit.max_retrieved_bytes,
        )
}

fn exceeds_optional(consumed: u64, reserved: u64, limit: Option<u64>) -> bool {
    limit.is_some_and(|limit| consumed.saturating_add(reserved) > limit)
}

fn validate_new_run(scope: ExecutionScope, new_run: &NewExecutionRun) -> Result<()> {
    if new_run
        .contact_id
        .is_some_and(|contact_id| contact_id.0.is_nil())
    {
        return Err(Error::InvalidRepositoryInput {
            message: "execution run contact_id must not be nil".to_string(),
        });
    }
    if !scope.permits_owner(new_run.tenant_id, new_run.contact_id) {
        return Err(Error::InvalidRepositoryInput {
            message: "run owner does not match the repository scope".to_string(),
        });
    }
    if !matches!(
        new_run.status,
        ExecutionRunStatus::AwaitingConfirmation | ExecutionRunStatus::Queued
    ) {
        return Err(Error::InvalidRepositoryInput {
            message: "new runs must start awaiting_confirmation or queued".to_string(),
        });
    }
    if new_run.plan.estimate.tasks == 0 {
        return Err(Error::InvalidRepositoryInput {
            message: "a canonical run plan must estimate at least one logical task".to_string(),
        });
    }
    if new_run.catalog.catalog_hash != new_run.plan.catalog_hash {
        return Err(Error::InvalidRepositoryInput {
            message: "persisted catalog hash does not match the canonical plan".to_string(),
        });
    }
    new_run
        .source_provenance
        .validate(&new_run.plan.plan_hash.to_string())
        .map_err(|error| Error::InvalidRepositoryInput {
            message: format!("invalid execution source provenance: {error}"),
        })?;
    let mut pinned = new_run.pinned_instruction_skills.clone();
    pinned.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });
    if pinned != new_run.pinned_instruction_skills
        || pinned.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(Error::InvalidRepositoryInput {
            message: "pinned instruction skills must be sorted and duplicate-free".to_string(),
        });
    }
    if new_run
        .pinned_instruction_skills
        .iter()
        .any(|pinned| !new_run.authorization.skill_refs.contains(&pinned.skill_ref))
    {
        return Err(Error::InvalidRepositoryInput {
            message: "pinned instruction skills must be present in the authorization envelope"
                .to_string(),
        });
    }
    Ok(())
}

struct NormalizedSourceFields<'a> {
    kind: ExecutionSourceKind,
    skill_template_ref: Option<&'a str>,
    skill_template_revision_uid: Option<Uuid>,
}

fn normalized_source_fields(provenance: &ExecutionSourceProvenance) -> NormalizedSourceFields<'_> {
    match provenance {
        ExecutionSourceProvenance::GeneratedPlan { .. } => NormalizedSourceFields {
            kind: ExecutionSourceKind::GeneratedPlan,
            skill_template_ref: None,
            skill_template_revision_uid: None,
        },
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
        } => NormalizedSourceFields {
            kind: ExecutionSourceKind::SkillTemplate,
            skill_template_ref: Some(skill_template_ref.as_str()),
            skill_template_revision_uid: Some(*skill_template_revision_uid),
        },
        ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => NormalizedSourceFields {
            kind: ExecutionSourceKind::ExperimentTemplate,
            skill_template_ref: Some(skill_template_ref.as_str()),
            skill_template_revision_uid: Some(*skill_template_revision_uid),
        },
    }
}

const fn route_source_label(source: ExecutionRouteSource) -> &'static str {
    match source {
        ExecutionRouteSource::Classifier => "classifier",
        ExecutionRouteSource::BlankObjective => "blank_objective",
        ExecutionRouteSource::SelectedExecutionTemplate => "selected_execution_template",
        ExecutionRouteSource::DurableUpgrade => "durable_upgrade",
    }
}

fn route_source_from_str(value: &str) -> Result<ExecutionRouteSource> {
    match value {
        "classifier" => Ok(ExecutionRouteSource::Classifier),
        "blank_objective" => Ok(ExecutionRouteSource::BlankObjective),
        "selected_execution_template" => Ok(ExecutionRouteSource::SelectedExecutionTemplate),
        "durable_upgrade" => Ok(ExecutionRouteSource::DurableUpgrade),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution route source `{value}`"),
        }),
    }
}

const fn route_classifier_outcome_label(outcome: ExecutionRouteClassifierOutcome) -> &'static str {
    match outcome {
        ExecutionRouteClassifierOutcome::NotCalled => "not_called",
        ExecutionRouteClassifierOutcome::Accepted => "accepted",
        ExecutionRouteClassifierOutcome::ProviderError => "provider_error",
        ExecutionRouteClassifierOutcome::StreamError => "stream_error",
        ExecutionRouteClassifierOutcome::Oversized => "oversized",
        ExecutionRouteClassifierOutcome::SchemaRejected => "schema_rejected",
        ExecutionRouteClassifierOutcome::InvalidDecision => "invalid_decision",
        ExecutionRouteClassifierOutcome::LowConfidence => "low_confidence",
        ExecutionRouteClassifierOutcome::ContextForcedInline => "context_forced_inline",
    }
}

fn route_classifier_outcome_from_str(value: &str) -> Result<ExecutionRouteClassifierOutcome> {
    match value {
        "not_called" => Ok(ExecutionRouteClassifierOutcome::NotCalled),
        "accepted" => Ok(ExecutionRouteClassifierOutcome::Accepted),
        "provider_error" => Ok(ExecutionRouteClassifierOutcome::ProviderError),
        "stream_error" => Ok(ExecutionRouteClassifierOutcome::StreamError),
        "oversized" => Ok(ExecutionRouteClassifierOutcome::Oversized),
        "schema_rejected" => Ok(ExecutionRouteClassifierOutcome::SchemaRejected),
        "invalid_decision" => Ok(ExecutionRouteClassifierOutcome::InvalidDecision),
        "low_confidence" => Ok(ExecutionRouteClassifierOutcome::LowConfidence),
        "context_forced_inline" => Ok(ExecutionRouteClassifierOutcome::ContextForcedInline),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution route classifier outcome `{value}`"),
        }),
    }
}

#[derive(Clone, Debug)]
struct PersistedRouteAudit {
    audit_uid: Uuid,
    stage: ExecutionRouteStage,
    evidence: RouteAuditEvidence,
}

#[derive(Clone, Debug)]
struct PersistedPlannerAudit {
    audit_uid: Uuid,
    call_ordinal: u8,
    run_uid: Option<Uuid>,
    plan_revision: Option<u64>,
    provider_model: String,
    prompt_version: String,
    candidate_json: Option<String>,
    compiler_report: Option<String>,
    evidence: PlannerCallAuditEvidence,
}

impl PersistedPlannerAudit {
    fn semantically_matches(
        &self,
        audit_uid: Uuid,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> bool {
        let ExecutionPlanningAuditPayload::PlannerCall {
            call_kind,
            call_ordinal,
            run_uid,
            plan_revision,
            outcome,
            provider_model,
            prompt_version,
            candidate_hash,
            candidate_json,
            compiler_report,
            ..
        } = &envelope.payload
        else {
            return false;
        };
        self.audit_uid == audit_uid
            && self.evidence.call == *call_kind
            && self.call_ordinal == *call_ordinal
            && self.run_uid == *run_uid
            && self.plan_revision == *plan_revision
            && self.evidence.outcome == *outcome
            && self.provider_model == *provider_model
            && self.prompt_version == *prompt_version
            && self.evidence.candidate_hash == *candidate_hash
            && self.candidate_json == *candidate_json
            && self.compiler_report == *compiler_report
    }
}

#[derive(Clone, Debug)]
struct PersistedCompileAudit {
    audit_uid: Uuid,
    session_id: Option<SessionId>,
    originating_sequence: Option<u64>,
    run_uid: Option<Uuid>,
    plan_revision: Option<u64>,
    operation_key: String,
    validation_report: String,
    evidence: CompileAuditEvidence,
}

impl PersistedCompileAudit {
    fn semantically_matches(
        &self,
        audit_uid: Uuid,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> bool {
        let ExecutionPlanningAuditPayload::Compile {
            source,
            operation_key,
            run_uid,
            plan_revision,
            outcome,
            candidate_hash,
            final_plan_hash,
            validation_report,
            ..
        } = &envelope.payload
        else {
            return false;
        };
        self.audit_uid == audit_uid
            && self.session_id == envelope.session_id
            && self.originating_sequence == envelope.originating_sequence
            && self.run_uid == *run_uid
            && self.plan_revision == *plan_revision
            && self.evidence.source == *source
            && self.operation_key == *operation_key
            && self.evidence.outcome == *outcome
            && self.evidence.candidate_hash == *candidate_hash
            && self.evidence.final_plan_hash == *final_plan_hash
            && self.validation_report == *validation_report
    }
}

fn validate_audit_scope(
    scope: ExecutionScope,
    envelope: &ExecutionPlanningAuditEnvelope,
) -> Result<()> {
    validate_planning_audit_envelope(envelope).map_err(|error| Error::InvalidRepositoryInput {
        message: error.to_string(),
    })?;
    if envelope
        .contact_id
        .is_some_and(|contact_id| contact_id.0.is_nil())
        || !scope.permits_owner(envelope.tenant_id, envelope.contact_id)
    {
        return Err(Error::InvalidRepositoryInput {
            message: "planning audit scope does not match its normalized owner".to_string(),
        });
    }
    Ok(())
}

fn route_audit_uid(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    session_id: SessionId,
    originating_sequence: u64,
    stage: ExecutionRouteStage,
) -> Result<Uuid> {
    execution_audit_uid(
        "moa.execution.route-audit",
        &[
            Some(tenant_id.0.to_string()),
            contact_id.map(|value| value.0.to_string()),
            Some(session_id.0.to_string()),
            Some(originating_sequence.to_string()),
            Some(route_stage_label(stage).to_string()),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
fn planner_audit_uid(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    session_id: SessionId,
    originating_sequence: u64,
    run_uid: Option<Uuid>,
    plan_revision: Option<u64>,
    call_kind: ExecutionPlannerCallKind,
    call_ordinal: u8,
) -> Result<Uuid> {
    execution_audit_uid(
        "moa.execution.planner-audit",
        &[
            Some(tenant_id.0.to_string()),
            contact_id.map(|value| value.0.to_string()),
            Some(session_id.0.to_string()),
            Some(originating_sequence.to_string()),
            run_uid.map(|value| value.to_string()),
            plan_revision.map(|value| value.to_string()),
            Some(planner_call_label(call_kind).to_string()),
            Some(call_ordinal.to_string()),
        ],
    )
}

fn compile_audit_uid(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    source: ExecutionCompileSource,
    operation_key: &str,
) -> Result<Uuid> {
    execution_audit_uid(
        "moa.execution.compile-audit",
        &[
            Some(tenant_id.0.to_string()),
            contact_id.map(|value| value.0.to_string()),
            Some(compile_source_label(source).to_string()),
            Some(operation_key.to_string()),
        ],
    )
}

fn execution_audit_uid(domain: &str, fields: &[Option<String>]) -> Result<Uuid> {
    let mut preimage = domain.as_bytes().to_vec();
    for field in fields {
        let Some(field) = field else {
            preimage.push(0);
            continue;
        };
        let length = u32::try_from(field.len()).map_err(|_| Error::InvalidRepositoryInput {
            message: "execution audit UUID field exceeds u32 bytes".to_string(),
        })?;
        preimage.push(1);
        preimage.extend_from_slice(&length.to_be_bytes());
        preimage.extend_from_slice(field.as_bytes());
    }
    Ok(Uuid::new_v5(&EXECUTION_AUDIT_NAMESPACE, &preimage))
}

fn route_audit_from_row(row: &PgRow) -> Result<PersistedRouteAudit> {
    let audit_uid: Uuid = row.try_get("audit_uid").map_err(row_error)?;
    let decision =
        route_decision_from_str(&row.try_get::<String, _>("decision").map_err(row_error)?)?;
    let strategy = row
        .try_get::<Option<String>, _>("strategy")
        .map_err(row_error)?
        .map(|value| execution_strategy_from_str(&value))
        .transpose()?;
    let confidence_bps = row
        .try_get::<Option<i16>, _>("confidence_bps")
        .map_err(row_error)?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| Error::InvalidRepositoryData {
            message: "route confidence is outside u16".to_string(),
        })?;
    let missing_input_count = u8::try_from(
        row.try_get::<i16, _>("missing_input_count")
            .map_err(row_error)?,
    )
    .map_err(|_| Error::InvalidRepositoryData {
        message: "route missing-input count is outside u8".to_string(),
    })?;
    Ok(PersistedRouteAudit {
        audit_uid,
        stage: route_stage_from_str(&row.try_get::<String, _>("stage").map_err(row_error)?)?,
        evidence: RouteAuditEvidence {
            audit_uid,
            decision,
            strategy,
            provenance: ExecutionRouteProvenance {
                source: route_source_from_str(
                    &row.try_get::<String, _>("source").map_err(row_error)?,
                )?,
                classifier_outcome: route_classifier_outcome_from_str(
                    &row.try_get::<String, _>("classifier_outcome")
                        .map_err(row_error)?,
                )?,
                provider_model: row.try_get("provider_model").map_err(row_error)?,
                prompt_version: row.try_get("prompt_version").map_err(row_error)?,
                objective_hash: row.try_get("objective_hash").map_err(row_error)?,
                response_hash: row.try_get("response_hash").map_err(row_error)?,
                confidence_bps,
                missing_input_count,
                usage: ExecutionRouteUsage {
                    input_tokens_uncached: required_u64(row, "input_tokens_uncached")?,
                    input_tokens_cache_write: required_u64(row, "input_tokens_cache_write")?,
                    input_tokens_cache_read: required_u64(row, "input_tokens_cache_read")?,
                    output_tokens: required_u64(row, "output_tokens")?,
                },
                cost_microusd: required_u64(row, "cost_microusd")?,
                duration_micros: required_u64(row, "duration_micros")?,
            },
            accepted_at: row.try_get("accepted_at").map_err(row_error)?,
        },
    })
}

fn planner_audit_from_row(row: &PgRow) -> Result<PersistedPlannerAudit> {
    let audit_uid: Uuid = row.try_get("audit_uid").map_err(row_error)?;
    let call = planner_call_from_str(&row.try_get::<String, _>("call_kind").map_err(row_error)?)?;
    let outcome =
        planner_outcome_from_str(&row.try_get::<String, _>("outcome").map_err(row_error)?)?;
    let call_ordinal = u8::try_from(row.try_get::<i16, _>("call_ordinal").map_err(row_error)?)
        .map_err(|_| Error::InvalidRepositoryData {
            message: "planner call ordinal is outside u8".to_string(),
        })?;
    Ok(PersistedPlannerAudit {
        audit_uid,
        call_ordinal,
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        plan_revision: optional_u64(row, "plan_revision")?,
        provider_model: row.try_get("provider_model").map_err(row_error)?,
        prompt_version: row.try_get("prompt_version").map_err(row_error)?,
        candidate_json: row.try_get("candidate_json").map_err(row_error)?,
        compiler_report: row.try_get("compiler_report").map_err(row_error)?,
        evidence: PlannerCallAuditEvidence {
            audit_uid,
            call,
            outcome,
            duration_micros: required_u64(row, "duration_micros")?,
            candidate_hash: row.try_get("candidate_hash").map_err(row_error)?,
        },
    })
}

fn compile_audit_from_row(row: &PgRow) -> Result<PersistedCompileAudit> {
    let audit_uid: Uuid = row.try_get("audit_uid").map_err(row_error)?;
    let session_id = row
        .try_get::<Option<Uuid>, _>("session_id")
        .map_err(row_error)?
        .map(SessionId);
    Ok(PersistedCompileAudit {
        audit_uid,
        session_id,
        originating_sequence: optional_u64(row, "originating_sequence")?,
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        plan_revision: optional_u64(row, "plan_revision")?,
        operation_key: row.try_get("operation_key").map_err(row_error)?,
        validation_report: row.try_get("validation_report").map_err(row_error)?,
        evidence: CompileAuditEvidence {
            audit_uid,
            source: compile_source_from_str(
                &row.try_get::<String, _>("source").map_err(row_error)?,
            )?,
            outcome: compile_outcome_from_str(
                &row.try_get::<String, _>("outcome").map_err(row_error)?,
            )?,
            duration_micros: required_u64(row, "duration_micros")?,
            candidate_hash: row.try_get("candidate_hash").map_err(row_error)?,
            final_plan_hash: row.try_get("final_plan_hash").map_err(row_error)?,
        },
    })
}

const fn route_stage_label(stage: ExecutionRouteStage) -> &'static str {
    match stage {
        ExecutionRouteStage::Initial => "initial",
        ExecutionRouteStage::DurableUpgrade => "durable_upgrade",
    }
}

fn route_stage_from_str(value: &str) -> Result<ExecutionRouteStage> {
    match value {
        "initial" => Ok(ExecutionRouteStage::Initial),
        "durable_upgrade" => Ok(ExecutionRouteStage::DurableUpgrade),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution route stage `{value}`"),
        }),
    }
}

const fn route_decision_label(decision: ExecutionRouteKind) -> &'static str {
    match decision {
        ExecutionRouteKind::Respond => "respond",
        ExecutionRouteKind::Execute => "execute",
        ExecutionRouteKind::NeedsInput => "needs_input",
    }
}

fn route_decision_from_str(value: &str) -> Result<ExecutionRouteKind> {
    match value {
        "respond" => Ok(ExecutionRouteKind::Respond),
        "execute" => Ok(ExecutionRouteKind::Execute),
        "needs_input" => Ok(ExecutionRouteKind::NeedsInput),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution route decision `{value}`"),
        }),
    }
}

const fn execution_strategy_label(strategy: ExecutionStrategy) -> &'static str {
    match strategy {
        ExecutionStrategy::Inline => "inline",
        ExecutionStrategy::Durable => "durable",
    }
}

fn execution_strategy_from_str(value: &str) -> Result<ExecutionStrategy> {
    match value {
        "inline" => Ok(ExecutionStrategy::Inline),
        "durable" => Ok(ExecutionStrategy::Durable),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution strategy `{value}`"),
        }),
    }
}

const fn planner_call_label(call: ExecutionPlannerCallKind) -> &'static str {
    match call {
        ExecutionPlannerCallKind::InitialPlan => "initial_plan",
        ExecutionPlannerCallKind::InitialRepair => "initial_repair",
        ExecutionPlannerCallKind::Amendment => "amendment",
        ExecutionPlannerCallKind::AmendmentRepair => "amendment_repair",
    }
}

fn planner_call_from_str(value: &str) -> Result<ExecutionPlannerCallKind> {
    match value {
        "initial_plan" => Ok(ExecutionPlannerCallKind::InitialPlan),
        "initial_repair" => Ok(ExecutionPlannerCallKind::InitialRepair),
        "amendment" => Ok(ExecutionPlannerCallKind::Amendment),
        "amendment_repair" => Ok(ExecutionPlannerCallKind::AmendmentRepair),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution planner call `{value}`"),
        }),
    }
}

const fn planner_outcome_label(outcome: ExecutionPlannerOutcome) -> &'static str {
    match outcome {
        ExecutionPlannerOutcome::Accepted => "accepted",
        ExecutionPlannerOutcome::NeedsInput => "needs_input",
        ExecutionPlannerOutcome::Unsupported => "unsupported",
        ExecutionPlannerOutcome::SchemaRejected => "schema_rejected",
        ExecutionPlannerOutcome::ImmutableGoalChanged => "immutable_goal_changed",
        ExecutionPlannerOutcome::CompilerRejected => "compiler_rejected",
        ExecutionPlannerOutcome::Oversized => "oversized",
        ExecutionPlannerOutcome::ProviderError => "provider_error",
    }
}

fn planner_outcome_from_str(value: &str) -> Result<ExecutionPlannerOutcome> {
    match value {
        "accepted" => Ok(ExecutionPlannerOutcome::Accepted),
        "needs_input" => Ok(ExecutionPlannerOutcome::NeedsInput),
        "unsupported" => Ok(ExecutionPlannerOutcome::Unsupported),
        "schema_rejected" => Ok(ExecutionPlannerOutcome::SchemaRejected),
        "immutable_goal_changed" => Ok(ExecutionPlannerOutcome::ImmutableGoalChanged),
        "compiler_rejected" => Ok(ExecutionPlannerOutcome::CompilerRejected),
        "oversized" => Ok(ExecutionPlannerOutcome::Oversized),
        "provider_error" => Ok(ExecutionPlannerOutcome::ProviderError),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution planner outcome `{value}`"),
        }),
    }
}

const fn compile_source_label(source: ExecutionCompileSource) -> &'static str {
    match source {
        ExecutionCompileSource::GeneratedPlan => "generated_plan",
        ExecutionCompileSource::SkillTemplate => "skill_template",
        ExecutionCompileSource::ExperimentTemplate => "experiment_template",
        ExecutionCompileSource::Amendment => "amendment",
        ExecutionCompileSource::SkillRegression => "skill_regression",
    }
}

fn compile_source_from_str(value: &str) -> Result<ExecutionCompileSource> {
    match value {
        "generated_plan" => Ok(ExecutionCompileSource::GeneratedPlan),
        "skill_template" => Ok(ExecutionCompileSource::SkillTemplate),
        "experiment_template" => Ok(ExecutionCompileSource::ExperimentTemplate),
        "amendment" => Ok(ExecutionCompileSource::Amendment),
        "skill_regression" => Ok(ExecutionCompileSource::SkillRegression),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution compile source `{value}`"),
        }),
    }
}

const fn compile_outcome_label(outcome: ExecutionCompileOutcome) -> &'static str {
    match outcome {
        ExecutionCompileOutcome::Accepted => "accepted",
        ExecutionCompileOutcome::NeedsInput => "needs_input",
        ExecutionCompileOutcome::Unsupported => "unsupported",
        ExecutionCompileOutcome::Rejected => "rejected",
    }
}

fn compile_outcome_from_str(value: &str) -> Result<ExecutionCompileOutcome> {
    match value {
        "accepted" => Ok(ExecutionCompileOutcome::Accepted),
        "needs_input" => Ok(ExecutionCompileOutcome::NeedsInput),
        "unsupported" => Ok(ExecutionCompileOutcome::Unsupported),
        "rejected" => Ok(ExecutionCompileOutcome::Rejected),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution compile outcome `{value}`"),
        }),
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

const INSERT_ROUTE_AUDIT_SQL: &str = r#"
    INSERT INTO moa.execution_route_audit (
        audit_uid, tenant_id, contact_id, session_id, originating_sequence,
        stage, decision, strategy, source, classifier_outcome,
        provider_model, prompt_version, objective_hash, response_hash,
        confidence_bps, missing_input_count, input_tokens_uncached,
        input_tokens_cache_write, input_tokens_cache_read, output_tokens,
        cost_microusd, duration_micros, accepted_at, created_at
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
        $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $23
    )
    ON CONFLICT DO NOTHING
    RETURNING
        audit_uid, stage, decision, strategy, source, classifier_outcome,
        provider_model, prompt_version, objective_hash, response_hash,
        confidence_bps, missing_input_count, input_tokens_uncached,
        input_tokens_cache_write, input_tokens_cache_read, output_tokens,
        cost_microusd, duration_micros, accepted_at
"#;

const LOAD_ROUTE_AUDIT_SQL: &str = r#"
    SELECT
        audit_uid, stage, decision, strategy, source, classifier_outcome,
        provider_model, prompt_version, objective_hash, response_hash,
        confidence_bps, missing_input_count, input_tokens_uncached,
        input_tokens_cache_write, input_tokens_cache_read, output_tokens,
        cost_microusd, duration_micros, accepted_at
    FROM moa.execution_route_audit
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND session_id = $3
      AND originating_sequence = $4
      AND stage = $5
"#;

const INSERT_PLANNER_AUDIT_SQL: &str = r#"
    INSERT INTO moa.execution_planner_call_audit (
        audit_uid, tenant_id, contact_id, session_id, originating_sequence,
        run_uid, plan_revision, call_kind, call_ordinal, outcome,
        provider_model, prompt_version, candidate_hash, candidate_json,
        compiler_report, duration_micros, created_at
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
        $11, $12, $13, $14::JSON, $15::JSON, $16, $17
    )
    ON CONFLICT DO NOTHING
    RETURNING
        audit_uid, run_uid, plan_revision, call_kind, call_ordinal, outcome,
        provider_model, prompt_version, candidate_hash,
        candidate_json::TEXT AS candidate_json,
        compiler_report::TEXT AS compiler_report,
        duration_micros
"#;

const LOAD_PLANNER_AUDIT_SQL: &str = r#"
    SELECT
        audit_uid, run_uid, plan_revision, call_kind, call_ordinal, outcome,
        provider_model, prompt_version, candidate_hash,
        candidate_json::TEXT AS candidate_json,
        compiler_report::TEXT AS compiler_report,
        duration_micros
    FROM moa.execution_planner_call_audit
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND session_id = $3
      AND originating_sequence = $4
      AND run_uid IS NOT DISTINCT FROM $5
      AND plan_revision IS NOT DISTINCT FROM $6
      AND call_kind = $7
      AND call_ordinal = $8
"#;

const INSERT_COMPILE_AUDIT_SQL: &str = r#"
    INSERT INTO moa.execution_compile_audit (
        audit_uid, tenant_id, contact_id, session_id, originating_sequence,
        run_uid, plan_revision, source, operation_key, outcome, candidate_hash,
        final_plan_hash, validation_report, duration_micros, created_at
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
        $11, $12, $13::JSON, $14, $15
    )
    ON CONFLICT DO NOTHING
    RETURNING
        audit_uid, session_id, originating_sequence, run_uid, plan_revision,
        source, operation_key, outcome, candidate_hash, final_plan_hash,
        validation_report::TEXT AS validation_report, duration_micros
"#;

const LOAD_COMPILE_AUDIT_SQL: &str = r#"
    SELECT
        audit_uid, session_id, originating_sequence, run_uid, plan_revision,
        source, operation_key, outcome, candidate_hash, final_plan_hash,
        validation_report::TEXT AS validation_report, duration_micros
    FROM moa.execution_compile_audit
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND source = $3
      AND operation_key = $4
"#;

const CREATE_PLANNING_CONTEXT_SQL: &str = r#"
    INSERT INTO moa.execution_planning_context (
        planning_context_uid, tenant_id, contact_id, session_id,
        originating_user_sequence_num, originating_user_event_hash,
        owner_user_id, planning_context_hash, snapshot
    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
    ON CONFLICT (tenant_id, session_id, originating_user_sequence_num) DO NOTHING
    RETURNING planning_context_uid, snapshot, planning_context_hash, created_at
"#;

const LOAD_PLANNING_CONTEXT_SQL: &str = r#"
    SELECT planning_context_uid, snapshot, planning_context_hash, created_at
    FROM moa.execution_planning_context
    WHERE planning_context_uid = $1
"#;

const LOAD_PLANNING_CONTEXT_BY_ORIGIN_SQL: &str = r#"
    SELECT planning_context_uid, snapshot, planning_context_hash, created_at
    FROM moa.execution_planning_context
    WHERE tenant_id = $1
      AND session_id = $2
      AND originating_user_sequence_num = $3
"#;

const CREATE_RUN_SQL: &str = r#"
    INSERT INTO moa.execution_run (
        run_uid, tenant_id, contact_id, session_id, originating_user_sequence_num,
        planning_context_uid, planning_context_hash, owner_user_id,
        goal_contract, initial_plan, active_plan, initial_plan_hash, active_plan_hash,
        capability_catalog, authorization_envelope, pinned_instruction_skills,
        source_provenance, source_kind, skill_template_ref,
        skill_template_revision_uid, input, status,
        budget_max_cost_microusd, budget_max_tokens, budget_max_tasks,
        budget_max_tool_calls, budget_max_retrieved_bytes, budget_deadline_at,
        progress_total_tasks, idempotency_key
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
        $14, $15, $16, $17, $18, $19, $20, $21, $22, $23,
        $24, $25, $26, $27, $28, $29, $30
    )
    ON CONFLICT (
        tenant_id,
        COALESCE(contact_id, '00000000-0000-0000-0000-000000000000'::UUID),
        idempotency_key
    ) WHERE idempotency_key IS NOT NULL
    DO NOTHING
    RETURNING *
"#;

const LOAD_RUN_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE run_uid = $1
"#;

const LIST_RUNS_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE (
        $1::TIMESTAMPTZ IS NULL
        OR (created_at, run_uid) < ($1, $2)
    )
    ORDER BY created_at DESC, run_uid DESC
    LIMIT $3
"#;

const LOAD_RUN_BY_IDEMPOTENCY_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND idempotency_key = $3
"#;

const LOAD_RUN_FOR_UPDATE_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE run_uid = $1
    FOR UPDATE
"#;

const CONFIRM_RUN_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = 'queued',
        queued_at = COALESCE(queued_at, NOW()),
        budget_max_cost_microusd = $3,
        budget_max_tokens = $4,
        budget_max_tasks = $5,
        budget_max_tool_calls = $6,
        budget_max_retrieved_bytes = $7,
        budget_deadline_at = $8,
        confirmed_plan_hash = $2,
        confirmed_at = NOW(),
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1
      AND status = 'awaiting_confirmation'
      AND active_plan_hash = $2
    RETURNING *
"#;

const INSERT_NODE_MATERIALIZATION_SQL: &str = r#"
    INSERT INTO moa.execution_node_materialization (
        run_uid, tenant_id, contact_id, plan_revision, node_id,
        kind, fanout_items, reducer_depth
    )
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
    ON CONFLICT (run_uid, plan_revision, node_id) DO NOTHING
    RETURNING kind, fanout_items, reducer_depth
"#;

const LOAD_NODE_MATERIALIZATION_SQL: &str = r#"
    SELECT kind, fanout_items, reducer_depth
    FROM moa.execution_node_materialization
    WHERE run_uid = $1 AND plan_revision = $2 AND node_id = $3
"#;

const INSERT_TASK_SQL: &str = r#"
    INSERT INTO moa.execution_task (
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes, generation_history
    ) VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, 'pending', 1, $9,
        $10, $11, $12, $13, $14, $15, $16, $17, $18
    )
    ON CONFLICT (run_uid, node_id, item_key) DO NOTHING
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const LOAD_TASK_BY_LOGICAL_KEY_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1 AND node_id = $2 AND item_key = $3
"#;

const LOAD_TASK_FOR_UPDATE_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1 AND task_id = $2
    FOR UPDATE
"#;

const LOAD_TASK_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1 AND task_id = $2
"#;

const RESERVE_RUN_BUDGET_SQL: &str = r#"
    UPDATE moa.execution_run
    SET reserved_cost_microusd = reserved_cost_microusd + $2,
        reserved_tokens = reserved_tokens + $3,
        reserved_tasks = reserved_tasks + $4,
        reserved_tool_calls = reserved_tool_calls + $5,
        reserved_retrieved_bytes = reserved_retrieved_bytes + $6,
        updated_at = NOW()
    WHERE run_uid = $1
      AND status IN ('queued', 'running')
      AND (budget_deadline_at IS NULL OR NOW() <= budget_deadline_at)
      AND reserved_cost_microusd <= 9223372036854775807 - $2
      AND reserved_tokens <= 9223372036854775807 - $3
      AND reserved_tasks <= 9223372036854775807 - $4
      AND reserved_tool_calls <= 9223372036854775807 - $5
      AND reserved_retrieved_bytes <= 9223372036854775807 - $6
      AND (
          budget_max_cost_microusd IS NULL
          OR consumed_cost_microusd::NUMERIC + reserved_cost_microusd::NUMERIC + $2::NUMERIC
             <= budget_max_cost_microusd::NUMERIC
      )
      AND (
          budget_max_tokens IS NULL
          OR consumed_tokens::NUMERIC + reserved_tokens::NUMERIC + $3::NUMERIC
             <= budget_max_tokens::NUMERIC
      )
      AND (
          budget_max_tasks IS NULL
          OR consumed_tasks::NUMERIC + reserved_tasks::NUMERIC + $4::NUMERIC
             <= budget_max_tasks::NUMERIC
      )
      AND (
          budget_max_tool_calls IS NULL
          OR consumed_tool_calls::NUMERIC + reserved_tool_calls::NUMERIC + $5::NUMERIC
             <= budget_max_tool_calls::NUMERIC
      )
      AND (
          budget_max_retrieved_bytes IS NULL
          OR consumed_retrieved_bytes::NUMERIC + reserved_retrieved_bytes::NUMERIC + $6::NUMERIC
             <= budget_max_retrieved_bytes::NUMERIC
      )
      AND NOT budget_overrun
"#;

const RESERVE_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'reserved',
        reserved_cost_microusd = $4,
        reserved_tokens = $5,
        reserved_tasks = $6,
        reserved_tool_calls = $7,
        reserved_retrieved_bytes = $8,
        reserved_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'pending'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const MARK_TASK_RUNNING_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'running', started_at = COALESCE(started_at, NOW()), updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'reserved'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const RESUME_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'running',
        attempt = $5,
        generation = $6,
        generation_history = generation_history || jsonb_build_array($7::JSONB),
        resume_input_history = CASE
            WHEN $8::JSONB IS NULL THEN resume_input_history
            ELSE resume_input_history || jsonb_build_array($8::JSONB)
        END,
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND status = $3 AND generation = $4
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const LIST_TASKS_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1
      AND (
          $2::TEXT IS NULL
          OR (node_id, item_key, task_id) > ($2, $3, $4)
      )
    ORDER BY node_id, item_key, task_id
    LIMIT $5
"#;

const LIST_ALL_TASKS_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1
    ORDER BY node_id, item_key, task_id
"#;

const RECONCILE_RUN_OUTCOME_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = $2,
        reserved_cost_microusd = $3,
        reserved_tokens = $4,
        reserved_tasks = $5,
        reserved_tool_calls = $6,
        reserved_retrieved_bytes = $7,
        consumed_cost_microusd = $8,
        consumed_tokens = $9,
        consumed_tasks = $10,
        consumed_tool_calls = $11,
        consumed_retrieved_bytes = $12,
        budget_overrun = $13,
        progress_completed_tasks = progress_completed_tasks + $14,
        progress_failed_tasks = progress_failed_tasks + $15,
        progress_cancelled_tasks = progress_cancelled_tasks + $16,
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1
    RETURNING *
"#;

const RECORD_TASK_OUTCOME_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = $4,
        reserved_cost_microusd = $5,
        reserved_tokens = $6,
        reserved_tasks = $7,
        reserved_tool_calls = $8,
        reserved_retrieved_bytes = $9,
        actual_cost_microusd = $10,
        actual_tokens = $11,
        actual_tasks = $12,
        actual_tool_calls = $13,
        actual_retrieved_bytes = $14,
        current_outcome = $15,
        output = $16,
        error = $17,
        citations = $18,
        outcome_audit = outcome_audit || jsonb_build_array($19::JSONB),
        completed_at = CASE WHEN $20 THEN COALESCE(completed_at, NOW()) ELSE NULL END,
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'running'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const RECORD_RESERVATION_REJECTION_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'failed',
        reserved_cost_microusd = 0,
        reserved_tokens = 0,
        reserved_tasks = 0,
        reserved_tool_calls = 0,
        reserved_retrieved_bytes = 0,
        actual_cost_microusd = $4,
        actual_tokens = $5,
        actual_tasks = 0,
        actual_tool_calls = $6,
        actual_retrieved_bytes = $7,
        current_outcome = $8,
        output = NULL,
        error = $9,
        citations = $10,
        outcome_audit = outcome_audit || jsonb_build_array($11::JSONB),
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'running'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const APPEND_TASK_OUTCOME_AUDIT_SQL: &str = r#"
    UPDATE moa.execution_task
    SET outcome_audit = outcome_audit || jsonb_build_array($3::JSONB),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const SUPERSEDE_REPLAN_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'cancelled',
        current_outcome = $4,
        reserved_cost_microusd = 0,
        reserved_tokens = 0,
        reserved_tasks = 0,
        reserved_tool_calls = 0,
        reserved_retrieved_bytes = 0,
        actual_tasks = 1,
        error = jsonb_build_object(
            'class', 'cancelled',
            'message', 'superseded_by_plan_revision'
        ),
        outcome_audit = outcome_audit || jsonb_build_array($3::JSONB),
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND status = 'waiting_replan'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const TERMINALIZE_CANCELLED_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'cancelled',
        reserved_cost_microusd = 0,
        reserved_tokens = 0,
        reserved_tasks = 0,
        reserved_tool_calls = 0,
        reserved_retrieved_bytes = 0,
        actual_tasks = 1,
        current_outcome = $4,
        output = NULL,
        error = $5,
        citations = $6,
        outcome_audit = outcome_audit || jsonb_build_array($7::JSONB),
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3
      AND status IN ('pending', 'reserved', 'running', 'waiting_input', 'waiting_replan')
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

const FINALIZE_REPLAN_STOP_RUN_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = $3,
        output = $4,
        completion_check_results = $5,
        terminal_gaps = $6,
        terminal_cause = $7,
        terminal_satisfied_requirement_count = $8,
        terminal_requirement_count = $9,
        terminal_reason = $10,
        waiting_reasons = '[]'::JSONB,
        reserved_cost_microusd = $11,
        reserved_tokens = $12,
        reserved_tasks = $13,
        reserved_tool_calls = $14,
        reserved_retrieved_bytes = $15,
        consumed_cost_microusd = $16,
        consumed_tokens = $17,
        consumed_tasks = $18,
        consumed_tool_calls = $19,
        consumed_retrieved_bytes = $20,
        budget_overrun = $21,
        progress_cancelled_tasks = progress_cancelled_tasks + $22,
        wake_epoch = wake_epoch + 1,
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND plan_revision = $2 AND status = 'waiting_replan'
    RETURNING *
"#;

const APPEND_AMENDMENT_SQL: &str = r#"
    UPDATE moa.execution_run
    SET active_plan = $4,
        active_plan_hash = $5,
        plan_revision = $3,
        plan_history = plan_history || jsonb_build_array($6::JSONB),
        status = 'running',
        reserved_cost_microusd = $7,
        reserved_tokens = $8,
        reserved_tasks = $9,
        reserved_tool_calls = $10,
        reserved_retrieved_bytes = $11,
        consumed_tasks = $12,
        budget_overrun = $13,
        progress_cancelled_tasks = progress_cancelled_tasks + 1,
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1 AND plan_revision = $2 AND status = 'waiting_replan'
    RETURNING *
"#;

const LOAD_NONTERMINAL_TASKS_FOR_UPDATE_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1
      AND status IN ('pending', 'reserved', 'running', 'waiting_input', 'waiting_replan')
    ORDER BY task_id
    FOR UPDATE
"#;

const CANCEL_RUN_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = 'cancelled',
        cancellation_reason = $2,
        terminal_cause = $3,
        terminal_reason = 'cancelled',
        terminal_satisfied_requirement_count = $4,
        terminal_requirement_count = $5,
        reserved_cost_microusd = $6,
        reserved_tokens = $7,
        reserved_tasks = $8,
        reserved_tool_calls = $9,
        reserved_retrieved_bytes = $10,
        consumed_cost_microusd = $11,
        consumed_tokens = $12,
        consumed_tasks = $13,
        consumed_tool_calls = $14,
        consumed_retrieved_bytes = $15,
        budget_overrun = $16,
        progress_cancelled_tasks = progress_cancelled_tasks + $17,
        waiting_reasons = '[]'::JSONB,
        wake_epoch = wake_epoch + 1,
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1
    RETURNING *
"#;

fn planning_context_from_row(row: &PgRow) -> Result<ExecutionPlanningContextRecord> {
    let snapshot: Value = row.try_get("snapshot").map_err(row_error)?;
    Ok(ExecutionPlanningContextRecord {
        planning_context_uid: row.try_get("planning_context_uid").map_err(row_error)?,
        snapshot: serde_json::from_value(snapshot)?,
        planning_context_hash: row
            .try_get::<String, _>("planning_context_hash")
            .map_err(row_error)?
            .parse()?,
        created_at: row.try_get("created_at").map_err(row_error)?,
    })
}

fn run_from_row(row: &PgRow) -> Result<ExecutionRunRecord> {
    let run_uid: Uuid = row.try_get("run_uid").map_err(row_error)?;
    let goal_value: Value = row.try_get("goal_contract").map_err(row_error)?;
    let initial_plan_value: Value = row.try_get("initial_plan").map_err(row_error)?;
    let active_plan_value: Value = row.try_get("active_plan").map_err(row_error)?;
    let plan_history: Value = row.try_get("plan_history").map_err(row_error)?;
    let catalog: Value = row.try_get("capability_catalog").map_err(row_error)?;
    let authorization: Value = row.try_get("authorization_envelope").map_err(row_error)?;
    let pinned_skills: Value = row
        .try_get("pinned_instruction_skills")
        .map_err(row_error)?;
    let completion_results: Value = row.try_get("completion_check_results").map_err(row_error)?;
    let terminal_gaps: Value = row.try_get("terminal_gaps").map_err(row_error)?;
    let terminal_cause: Option<Value> = row.try_get("terminal_cause").map_err(row_error)?;
    let terminal_satisfied_requirement_count =
        optional_u64(row, "terminal_satisfied_requirement_count")?;
    let terminal_requirement_count = optional_u64(row, "terminal_requirement_count")?;
    let terminal_evidence = match (
        terminal_cause,
        terminal_satisfied_requirement_count,
        terminal_requirement_count,
    ) {
        (None, None, None) => None,
        (Some(cause), Some(satisfied_requirement_count), Some(requirement_count)) => {
            Some(ExecutionTerminalEvidence {
                cause: serde_json::from_value(cause)?,
                satisfied_requirement_count,
                requirement_count,
            })
        }
        _ => {
            return Err(Error::InvalidRepositoryData {
                message: "execution terminal evidence columns are only partially populated"
                    .to_string(),
            });
        }
    };
    let waiting_reasons: Value = row.try_get("waiting_reasons").map_err(row_error)?;
    let source_provenance: ExecutionSourceProvenance =
        serde_json::from_value(row.try_get("source_provenance").map_err(row_error)?)?;
    let source_kind = ExecutionSourceKind::from_str(
        &row.try_get::<String, _>("source_kind").map_err(row_error)?,
    )?;
    let status =
        ExecutionRunStatus::from_str(&row.try_get::<String, _>("status").map_err(row_error)?)?;
    let terminal_reason = row
        .try_get::<Option<String>, _>("terminal_reason")
        .map_err(row_error)?
        .map(|value| ExecutionTerminalReason::from_str(&value))
        .transpose()?;
    if status.is_terminal() != terminal_reason.is_some() {
        return Err(Error::InvalidRepositoryData {
            message: "execution terminal reason nullability disagrees with run status".to_string(),
        });
    }
    let contact_id: Option<Uuid> = row.try_get("contact_id").map_err(row_error)?;
    let session_id: Uuid = row.try_get("session_id").map_err(row_error)?;
    let owner_user_id: String = row.try_get("owner_user_id").map_err(row_error)?;
    Ok(ExecutionRunRecord {
        run_uid,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(row_error)?),
        contact_id: contact_id.map(ContactId),
        session_id: SessionId(session_id),
        originating_user_sequence_num: required_u64(row, "originating_user_sequence_num")?,
        planning_context_uid: row.try_get("planning_context_uid").map_err(row_error)?,
        planning_context_hash: row
            .try_get::<String, _>("planning_context_hash")
            .map_err(row_error)?
            .parse()?,
        owner_user_id: UserId::new(owner_user_id),
        goal: serde_json::from_value(goal_value)?,
        initial_plan: serde_json::from_value(initial_plan_value)?,
        active_plan: serde_json::from_value(active_plan_value)?,
        initial_plan_hash: row
            .try_get::<String, _>("initial_plan_hash")
            .map_err(row_error)?
            .parse()?,
        active_plan_hash: row
            .try_get::<String, _>("active_plan_hash")
            .map_err(row_error)?
            .parse()?,
        confirmed_plan_hash: row
            .try_get::<Option<String>, _>("confirmed_plan_hash")
            .map_err(row_error)?
            .map(|value| value.parse())
            .transpose()?,
        plan_revision: to_u64(
            row.try_get("plan_revision").map_err(row_error)?,
            "plan revision",
        )?,
        plan_history: serde_json::from_value(plan_history)?,
        catalog: serde_json::from_value(catalog)?,
        authorization: serde_json::from_value(authorization)?,
        pinned_instruction_skills: serde_json::from_value(pinned_skills)?,
        source_provenance,
        source_kind,
        skill_template_ref: row.try_get("skill_template_ref").map_err(row_error)?,
        skill_template_revision_uid: row
            .try_get("skill_template_revision_uid")
            .map_err(row_error)?,
        input: row.try_get("input").map_err(row_error)?,
        output: row.try_get("output").map_err(row_error)?,
        completion_check_results: serde_json::from_value(completion_results)?,
        terminal_gaps: serde_json::from_value(terminal_gaps)?,
        terminal_evidence,
        terminal_reason,
        status,
        approved_budget: ExecutionBudgetLimit {
            max_cost_microusd: optional_u64(row, "budget_max_cost_microusd")?,
            max_tokens: optional_u64(row, "budget_max_tokens")?,
            max_tasks: optional_u64(row, "budget_max_tasks")?,
            max_tool_calls: optional_u64(row, "budget_max_tool_calls")?,
            max_retrieved_bytes: optional_u64(row, "budget_max_retrieved_bytes")?,
            deadline_at: row.try_get("budget_deadline_at").map_err(row_error)?,
        },
        reserved: estimate_from_row(row, "reserved")?,
        consumed: estimate_from_row(row, "consumed")?,
        budget_overrun: row.try_get("budget_overrun").map_err(row_error)?,
        progress_total_tasks: required_u64(row, "progress_total_tasks")?,
        progress_completed_tasks: required_u64(row, "progress_completed_tasks")?,
        progress_failed_tasks: required_u64(row, "progress_failed_tasks")?,
        progress_cancelled_tasks: required_u64(row, "progress_cancelled_tasks")?,
        waiting_reasons: serde_json::from_value(waiting_reasons)?,
        wake_epoch: required_u64(row, "wake_epoch")?,
        processed_wake_epoch: required_u64(row, "processed_wake_epoch")?,
        idempotency_key: row.try_get("idempotency_key").map_err(row_error)?,
        cancellation_reason: row.try_get("cancellation_reason").map_err(row_error)?,
        created_at: row.try_get("created_at").map_err(row_error)?,
        queued_at: row.try_get("queued_at").map_err(row_error)?,
        updated_at: row.try_get("updated_at").map_err(row_error)?,
        started_at: row.try_get("started_at").map_err(row_error)?,
        completed_at: row.try_get("completed_at").map_err(row_error)?,
        confirmed_at: row.try_get("confirmed_at").map_err(row_error)?,
    })
}

fn task_from_row(row: &PgRow) -> Result<ExecutionTaskRecord> {
    let contact_id: Option<Uuid> = row.try_get("contact_id").map_err(row_error)?;
    let requirement_ids: Value = row.try_get("requirement_ids").map_err(row_error)?;
    let kind: Value = row.try_get("task_kind").map_err(row_error)?;
    let retry: Value = row.try_get("retry_policy").map_err(row_error)?;
    let resume_input_history: Value = row.try_get("resume_input_history").map_err(row_error)?;
    let current_outcome: Option<Value> = row.try_get("current_outcome").map_err(row_error)?;
    let citations: Value = row.try_get("citations").map_err(row_error)?;
    let generation_history: Value = row.try_get("generation_history").map_err(row_error)?;
    let outcome_audit: Value = row.try_get("outcome_audit").map_err(row_error)?;
    let actual = estimate_from_row(row, "actual")?;
    Ok(ExecutionTaskRecord {
        task_id: ExecutionTaskId::from_uuid(row.try_get("task_id").map_err(row_error)?),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(row_error)?),
        contact_id: contact_id.map(ContactId),
        node_id: row.try_get("node_id").map_err(row_error)?,
        item_key: row.try_get("item_key").map_err(row_error)?,
        requirement_ids: serde_json::from_value(requirement_ids)?,
        plan_revision: required_u64(row, "plan_revision")?,
        status: ExecutionTaskStatus::from_str(
            &row.try_get::<String, _>("status").map_err(row_error)?,
        )?,
        attempt: to_u32(row.try_get("attempt").map_err(row_error)?, "attempt")?,
        generation: required_u64(row, "generation")?,
        input: row.try_get("input").map_err(row_error)?,
        resume_input_history: serde_json::from_value(resume_input_history)?,
        kind: serde_json::from_value(kind)?,
        retry: serde_json::from_value(retry)?,
        estimate: estimate_from_row(row, "estimate")?,
        reserved: estimate_from_row(row, "reserved")?,
        actual: ExecutionUsage {
            cost_microusd: actual.cost_microusd,
            tokens: actual.tokens,
            tool_calls: actual.tool_calls,
            retrieved_bytes: actual.retrieved_bytes,
        },
        actual_tasks: actual.tasks,
        current_outcome: current_outcome.map(serde_json::from_value).transpose()?,
        output: row.try_get("output").map_err(row_error)?,
        error: row.try_get("error").map_err(row_error)?,
        citations: serde_json::from_value(citations)?,
        generation_history: serde_json::from_value(generation_history)?,
        outcome_audit: serde_json::from_value(outcome_audit)?,
        created_at: row.try_get("created_at").map_err(row_error)?,
        updated_at: row.try_get("updated_at").map_err(row_error)?,
        reserved_at: row.try_get("reserved_at").map_err(row_error)?,
        started_at: row.try_get("started_at").map_err(row_error)?,
        completed_at: row.try_get("completed_at").map_err(row_error)?,
    })
}

fn optional_u64(row: &PgRow, column: &str) -> Result<Option<u64>> {
    row.try_get::<Option<i64>, _>(column)
        .map_err(row_error)?
        .map(|value| to_u64(value, column))
        .transpose()
}

fn required_u64(row: &PgRow, column: &str) -> Result<u64> {
    to_u64(row.try_get(column).map_err(row_error)?, column)
}

fn estimate_from_row(row: &PgRow, prefix: &str) -> Result<ExecutionEstimate> {
    Ok(ExecutionEstimate {
        cost_microusd: required_u64(row, &format!("{prefix}_cost_microusd"))?,
        tokens: required_u64(row, &format!("{prefix}_tokens"))?,
        tasks: required_u64(row, &format!("{prefix}_tasks"))?,
        tool_calls: required_u64(row, &format!("{prefix}_tool_calls"))?,
        retrieved_bytes: required_u64(row, &format!("{prefix}_retrieved_bytes"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_route_repository_parsers_accept_exact_current_values() {
        // Pins: repository reconstruction accepts every value in the normalized
        // decision, strategy, source, stage, and classifier-outcome contract.
        for (value, expected) in [
            ("respond", ExecutionRouteKind::Respond),
            ("execute", ExecutionRouteKind::Execute),
            ("needs_input", ExecutionRouteKind::NeedsInput),
        ] {
            assert_eq!(
                route_decision_from_str(value).expect("current decision should parse"),
                expected
            );
        }
        for (value, expected) in [
            ("inline", ExecutionStrategy::Inline),
            ("durable", ExecutionStrategy::Durable),
        ] {
            assert_eq!(
                execution_strategy_from_str(value).expect("current strategy should parse"),
                expected
            );
        }
        for (value, expected) in [
            ("initial", ExecutionRouteStage::Initial),
            ("durable_upgrade", ExecutionRouteStage::DurableUpgrade),
        ] {
            assert_eq!(
                route_stage_from_str(value).expect("current stage should parse"),
                expected
            );
        }
        for (value, expected) in [
            ("classifier", ExecutionRouteSource::Classifier),
            ("blank_objective", ExecutionRouteSource::BlankObjective),
            (
                "selected_execution_template",
                ExecutionRouteSource::SelectedExecutionTemplate,
            ),
            ("durable_upgrade", ExecutionRouteSource::DurableUpgrade),
        ] {
            assert_eq!(
                route_source_from_str(value).expect("current source should parse"),
                expected
            );
        }
        for (value, expected) in [
            ("not_called", ExecutionRouteClassifierOutcome::NotCalled),
            ("accepted", ExecutionRouteClassifierOutcome::Accepted),
            (
                "provider_error",
                ExecutionRouteClassifierOutcome::ProviderError,
            ),
            ("stream_error", ExecutionRouteClassifierOutcome::StreamError),
            ("oversized", ExecutionRouteClassifierOutcome::Oversized),
            (
                "schema_rejected",
                ExecutionRouteClassifierOutcome::SchemaRejected,
            ),
            (
                "invalid_decision",
                ExecutionRouteClassifierOutcome::InvalidDecision,
            ),
            (
                "low_confidence",
                ExecutionRouteClassifierOutcome::LowConfidence,
            ),
            (
                "context_forced_inline",
                ExecutionRouteClassifierOutcome::ContextForcedInline,
            ),
        ] {
            assert_eq!(
                route_classifier_outcome_from_str(value)
                    .expect("current classifier outcome should parse"),
                expected
            );
        }
    }

    #[test]
    fn execution_route_repository_parsers_reject_removed_values() {
        // Pins: the breaking cutover does not translate any removed route value.
        for value in ["routed", "act", "run"] {
            assert!(route_decision_from_str(value).is_err(), "accepted {value}");
        }
        for value in ["respond", "act", "run"] {
            assert!(
                execution_strategy_from_str(value).is_err(),
                "accepted {value}"
            );
        }
        let removed_upgrade = ["act", "_escalation"].concat();
        assert!(route_stage_from_str(&removed_upgrade).is_err());
        assert!(route_source_from_str(&removed_upgrade).is_err());
        assert!(route_classifier_outcome_from_str(&["context_forced_", "act"].concat()).is_err());
    }
}
