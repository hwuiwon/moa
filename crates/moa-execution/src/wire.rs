//! Public execution service and internal durable-workflow wire contracts.

use chrono::{DateTime, Utc};
use moa_artifacts::{
    execution_plan::{
        ExecutionBudgetLimit, ExecutionFailureClass, ExecutionPlanTemplate, ExecutionTaskResult,
        InputAudience, PlanAmendment,
    },
    reference::ArtifactRef,
};
use moa_core::events::Event;
use moa_core::events::{
    ExecutionBlockerAudience, ExecutionFailureDisposition, ExecutionProgress,
    ExecutionProgressEconomics, ExecutionProgressPhase, ExecutionRemainingBudget,
    ExecutionTaskResultsRef, ExecutionTerminalSummary,
};
use moa_core::types::{
    contact::ContactId,
    execution_planning::{ExecutionSourceProvenance, PinnedExecutionTemplateRef},
    identifiers::{SessionId, TenantId, UserId},
    tools::{AsyncToolJob, AsyncToolJobCallbackOutcome, AsyncToolJobCancelOutcome},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    Error, Result,
    budget::BudgetLedger,
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash,
    },
    compiler::CompiledExecution,
    state::{
        CompensationId, ExecutionProjection, ExecutionRunStatus, ExecutionSourceKind,
        ExecutionTaskId, ExecutionTaskProjection, ExecutionTerminalEvidence,
        ExecutionTerminalReason, WaitingReason,
    },
};

const CURSOR_PREFIX: &str = "cursor:";
const BASE64_URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const PLANNING_CONTEXT_HASH_DOMAIN: &str = "moa.execution.planning-context";
const ORIGINATING_USER_EVENT_HASH_DOMAIN: &str = "moa.execution.originating-user-event";
const TEMPLATE_ADMISSION_OPERATION_DOMAIN: &str = "moa.execution.template-admission-operation";
const TEMPLATE_ADMISSION_REQUEST_DOMAIN: &str = "moa.execution.template-admission-request";
const TEMPLATE_ADMISSION_OPERATION_NAMESPACE: Uuid =
    Uuid::from_u128(0x8fc5_e5a4_2e5c_58dd_95d0_aad9_0d59_ae6d);
/// Maximum UTF-8 bytes accepted for an external execution-template admission key.
pub const EXECUTION_TEMPLATE_ADMISSION_IDEMPOTENCY_KEY_MAX_BYTES: usize = 256;
/// Maximum canonical terminal-output bytes copied into a session event.
pub const EXECUTION_TERMINAL_INLINE_OUTPUT_MAX_BYTES: usize = 16 * 1024;
/// Maximum citation identifiers retained in one terminal session event.
pub const EXECUTION_TERMINAL_MAX_CITATION_IDS: usize = 100;
/// Maximum failure summaries retained in one terminal session event.
pub const EXECUTION_TERMINAL_MAX_FAILURES: usize = 20;
/// Maximum completion gaps retained in one terminal session event.
pub const EXECUTION_TERMINAL_MAX_GAPS: usize = 50;
/// Maximum Unicode scalar values retained in one failure or gap summary.
pub const EXECUTION_TERMINAL_TEXT_MAX_CHARS: usize = 512;

/// Exact activated instruction-skill revision pinned into one execution run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedInstructionSkill {
    /// Stable activated skill reference.
    pub skill_ref: ArtifactRef,
    /// Exact immutable artifact revision.
    pub revision_uid: Uuid,
}

/// Exact published execution template pinned into one planning-context snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedExecutionTemplate {
    /// Stable activated skill reference.
    pub skill_ref: ArtifactRef,
    /// Exact immutable artifact revision.
    pub revision_uid: Uuid,
    /// Skill-level structured invocation schema.
    pub skill_input_schema: Value,
    /// Paired immutable goal and execution-plan template.
    pub execution_plan: ExecutionPlanTemplate,
}

/// Request to derive or replay one immutable origin-bound planning context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanningContextRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Authorized parent session.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that supplies the objective.
    pub originating_user_sequence_num: u64,
    /// Absolute authorized deadline frozen into the Durable planning context.
    pub deadline_at: DateTime<Utc>,
    /// Optional exact template selection hint; this grants no authority.
    pub requested_template: Option<PinnedExecutionTemplateRef>,
}

/// Exact authenticated request admitted through a parent Session execution template route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTemplateAdmissionRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Existing authorized parent Session.
    pub session_id: SessionId,
    /// Exact immutable activated skill-template revision.
    pub template: PinnedExecutionTemplateRef,
    /// User-authored objective appended to Session history before planning.
    pub objective: String,
    /// Structured template input.
    pub input: Value,
    /// Optional permanent tenant-scoped semantic idempotency key.
    pub idempotency_key: Option<String>,
}

/// Stable response from external execution-template admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTemplateAdmissionResponse {
    /// Existing parent Session used for admission.
    pub session_id: SessionId,
    /// Exact first persisted user-objective event sequence.
    pub originating_user_sequence_num: u64,
    /// Exact execution run created or replayed for the operation.
    pub execution_run_uid: Uuid,
}

impl ExecutionTemplateAdmissionRequest {
    /// Validates scope and idempotency fields that must be rejected before persistence.
    pub fn validate(&self) -> Result<()> {
        if self
            .contact_id
            .is_some_and(|contact_id| contact_id.0.is_nil())
        {
            return Err(Error::InvalidRepositoryInput {
                message: "execution-template admission contact_id must not be nil".to_string(),
            });
        }
        if let Some(key) = &self.idempotency_key {
            validate_execution_template_admission_key(key)?;
        }
        Ok(())
    }
}

/// Derives the permanent UUIDv5 operation identity for one non-null tenant/key tuple.
pub fn execution_template_admission_operation_uid(
    tenant_id: TenantId,
    idempotency_key: &str,
) -> Result<Uuid> {
    validate_execution_template_admission_key(idempotency_key)?;
    let mut name = TEMPLATE_ADMISSION_OPERATION_DOMAIN.as_bytes().to_vec();
    let tenant = tenant_id.to_string();
    append_nullable_frame(&mut name, Some(tenant.as_bytes()))?;
    append_nullable_frame(&mut name, Some(idempotency_key.as_bytes()))?;
    Ok(Uuid::new_v5(&TEMPLATE_ADMISSION_OPERATION_NAMESPACE, &name))
}

/// Computes the permanent semantic fingerprint of the complete canonical admission request.
pub fn execution_template_admission_request_fingerprint(
    request: &ExecutionTemplateAdmissionRequest,
) -> Result<ExecutionHash> {
    request.validate()?;
    let bytes = moa_core::canonical_json::canonical_json_bytes(&serde_json::json!({
        "schema_version": 1,
        "tenant_id": request.tenant_id,
        "contact_id": request.contact_id,
        "session_id": request.session_id,
        "template": request.template,
        "objective": request.objective,
        "input": request.input,
        "idempotency_key": request.idempotency_key,
    }))?;
    Ok(domain_hash(TEMPLATE_ADMISSION_REQUEST_DOMAIN, &bytes))
}

fn validate_execution_template_admission_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > EXECUTION_TEMPLATE_ADMISSION_IDEMPOTENCY_KEY_MAX_BYTES {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "execution-template admission idempotency key must contain 1..={EXECUTION_TEMPLATE_ADMISSION_IDEMPOTENCY_KEY_MAX_BYTES} UTF-8 bytes"
            ),
        });
    }
    Ok(())
}

fn append_nullable_frame(output: &mut Vec<u8>, value: Option<&[u8]>) -> Result<()> {
    let Some(value) = value else {
        output.push(0);
        return Ok(());
    };
    output.push(1);
    let length = u32::try_from(value.len()).map_err(|_| Error::InvalidRepositoryInput {
        message: "execution-template admission identity field exceeds four-byte framing"
            .to_string(),
    })?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}

/// Immutable authority and capability snapshot used by planning and admission.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanningContextSnapshot {
    /// Planning-context schema version, fixed at `1`.
    pub schema_version: u8,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Authorized parent session.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence.
    pub originating_user_sequence_num: u64,
    /// Domain-separated hash of the complete persisted user-message event.
    pub originating_user_event_hash: String,
    /// Authorized owning user derived from the parent session request.
    pub owner_user_id: UserId,
    /// Immutable capability catalog available to the compiler.
    pub catalog: ExecutionCapabilityCatalog,
    /// Immutable capability and skill authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Sorted exact instruction-skill revisions.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Sorted exact execution-template revisions.
    pub execution_templates: Vec<PinnedExecutionTemplate>,
    /// Maximum run budget derivable from current server policy.
    pub budget: ExecutionBudgetLimit,
}

/// Committed immutable planning-context record returned by `Execution/planning_context`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanningContextResponse {
    /// Durable planning-context identifier.
    pub planning_context_uid: Uuid,
    /// Domain-separated hash of the canonical snapshot bytes.
    pub planning_context_hash: String,
    /// Exact immutable snapshot.
    pub snapshot: ExecutionPlanningContextSnapshot,
    /// Whether this call inserted the snapshot rather than replaying the origin.
    pub created: bool,
}

impl ExecutionPlanningContextSnapshot {
    /// Validates snapshot version, hashes, deterministic pin ordering, and authorization closure.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(Error::InvalidRepositoryInput {
                message: "planning-context schema_version must equal 1".to_string(),
            });
        }
        self.originating_user_event_hash.parse::<ExecutionHash>()?;
        let skill_keys = self
            .pinned_instruction_skills
            .iter()
            .map(|skill| {
                Ok((
                    skill
                        .skill_ref
                        .canonical_string()
                        .map_err(artifact_contract_error)?,
                    skill.revision_uid,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut sorted_skill_keys = skill_keys.clone();
        sorted_skill_keys.sort();
        if skill_keys != sorted_skill_keys
            || sorted_skill_keys.windows(2).any(|pair| pair[0] == pair[1])
        {
            return Err(Error::InvalidRepositoryInput {
                message: "pinned instruction skills must be canonical, sorted, and unique"
                    .to_string(),
            });
        }
        let template_keys = self
            .execution_templates
            .iter()
            .map(|template| {
                Ok((
                    template
                        .skill_ref
                        .canonical_string()
                        .map_err(artifact_contract_error)?,
                    template.revision_uid,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut sorted_template_keys = template_keys.clone();
        sorted_template_keys.sort();
        if template_keys != sorted_template_keys
            || sorted_template_keys
                .windows(2)
                .any(|pair| pair[0] == pair[1])
        {
            return Err(Error::InvalidRepositoryInput {
                message: "execution templates must be canonical, sorted, and unique".to_string(),
            });
        }
        if self
            .pinned_instruction_skills
            .iter()
            .any(|skill| !self.authorization.skill_refs.contains(&skill.skill_ref))
            || self
                .execution_templates
                .iter()
                .any(|template| !self.authorization.skill_refs.contains(&template.skill_ref))
        {
            return Err(Error::InvalidRepositoryInput {
                message: "planning-context skill pins must be authorized".to_string(),
            });
        }
        Ok(())
    }
}

/// Computes the canonical domain-separated hash of one immutable planning snapshot.
pub fn planning_context_hash(snapshot: &ExecutionPlanningContextSnapshot) -> Result<ExecutionHash> {
    snapshot.validate()?;
    let bytes = moa_core::canonical_json::canonical_json_bytes(snapshot)?;
    Ok(domain_hash(PLANNING_CONTEXT_HASH_DOMAIN, &bytes))
}

/// Computes the canonical hash of the exact persisted originating user event.
pub fn originating_user_event_hash(
    session_id: SessionId,
    sequence: u64,
    event: &Event,
) -> Result<ExecutionHash> {
    let bytes = moa_core::canonical_json::canonical_json_bytes(&serde_json::json!({
        "schema_version": 1,
        "session_id": session_id,
        "sequence": sequence,
        "event": event,
    }))?;
    Ok(domain_hash(ORIGINATING_USER_EVENT_HASH_DOMAIN, &bytes))
}

fn domain_hash(domain: &str, bytes: &[u8]) -> ExecutionHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    ExecutionHash::from_bytes(*hasher.finalize().as_bytes())
}

fn artifact_contract_error(error: moa_artifacts::Error) -> Error {
    Error::InvalidRepositoryInput {
        message: error.to_string(),
    }
}

/// Request to create or replay one durable execution run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStartRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Parent session authorized before run creation.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that originated the run.
    pub originating_user_sequence_num: u64,
    /// Immutable planning-context row to load.
    pub planning_context_uid: Uuid,
    /// Expected canonical hash of the immutable planning context.
    pub planning_context_hash: String,
    /// Optional scope-local idempotency key.
    pub idempotency_key: Option<String>,
    /// Compiler-validated goal and canonical plan.
    pub compiled: CompiledExecution,
    /// Structured input consumed by the plan.
    pub run_input: Value,
    /// Closed source and compiler provenance.
    pub source_provenance: ExecutionSourceProvenance,
}

/// Parent-scoped identifier for one execution run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Parent session authorized before reading the run.
    pub session_id: SessionId,
    /// Durable run identifier.
    pub run_uid: Uuid,
}

/// Request to confirm the displayed active plan and approved budget.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfirmRequest {
    /// Parent-scoped run identifier.
    pub run: ExecutionRunRequest,
    /// Exact active-plan hash displayed to the user.
    pub expected_plan_hash: ExecutionHash,
    /// Newly approved resource envelope.
    pub approved_budget: ExecutionBudgetLimit,
}

/// Request to resume one task with audience-bound external input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionInputRequest {
    /// Owning tenant authorized before the run is read.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Required parent session for user-audience input.
    pub session_id: Option<SessionId>,
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Exact waiting task.
    pub task_id: ExecutionTaskId,
    /// Current generation fence.
    pub expected_generation: u64,
    /// Audience under which the input is authorized.
    pub audience: InputAudience,
    /// Exact payload appended to resume-input history.
    pub input: Value,
}

/// Request to resolve one explicit review task.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReviewDecisionRequest {
    /// Owning tenant authorized before the run is read.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Exact running review task.
    pub task_id: ExecutionTaskId,
    /// Current generation fence.
    pub expected_generation: u64,
    /// Typed tenant review decision.
    pub decision: ExecutionReviewDecision,
}

/// Typed decision for an explicit execution review node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionReviewDecision {
    /// Approve the node with a structured result payload.
    Approved {
        /// Structured review result.
        payload: Value,
    },
    /// Reject the node with a stable human-readable reason.
    Rejected {
        /// Rejection reason.
        reason: String,
    },
}

/// Request to deliver one named external signal.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSignalRequest {
    /// Owning tenant authorized before the run is read.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Exact running signal task.
    pub task_id: ExecutionTaskId,
    /// Current generation fence.
    pub expected_generation: u64,
    /// Expected signal name persisted in the task descriptor.
    pub signal_name: String,
    /// Structured signal payload.
    pub payload: Value,
}

/// Request to validate and append an externally supplied plan amendment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAmendmentRequest {
    /// Parent-scoped run identifier.
    pub run: ExecutionRunRequest,
    /// Active plan revision fence.
    pub expected_plan_revision: u64,
    /// Externally supplied amendment; Task 6 never generates this value.
    pub amendment: PlanAmendment,
}

/// Request to cancel one run and all nonterminal tasks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCancelRequest {
    /// Parent-scoped run identifier.
    pub run: ExecutionRunRequest,
    /// Persisted cancellation reason.
    pub reason: String,
}

/// Bounded tenant/contact run-list request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunListRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional exact contact scope.
    pub contact_id: Option<ContactId>,
    /// Requested page size.
    pub limit: Option<u32>,
    /// Opaque versioned keyset cursor.
    pub cursor: Option<String>,
}

/// Bounded task-list request for one parent-authorized run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskListRequest {
    /// Parent-scoped run identifier.
    pub run: ExecutionRunRequest,
    /// Requested page size.
    pub limit: Option<u32>,
    /// Opaque versioned keyset cursor.
    pub cursor: Option<String>,
}

/// Stable descending keyset cursor for run pages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunCursor {
    /// Creation time of the last returned run.
    pub created_at: DateTime<Utc>,
    /// Durable identifier of the last returned run.
    pub run_uid: Uuid,
}

/// Stable ascending keyset cursor for task pages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskCursor {
    /// Node identifier of the last returned task.
    pub node_id: String,
    /// Item key of the last returned task.
    pub item_key: String,
    /// Stable identifier of the last returned task.
    pub task_id: ExecutionTaskId,
}

/// Aggregate durable run projection returned by service APIs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunSummary {
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Parent session identifier.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that originated the run.
    pub originating_user_sequence_num: u64,
    /// Current durable status.
    pub status: ExecutionRunStatus,
    /// Normalized source cohort.
    pub source_kind: ExecutionSourceKind,
    /// Exact pinned skill-template reference, when template-backed.
    pub skill_template_ref: Option<String>,
    /// Exact pinned skill-template revision, when template-backed.
    pub skill_template_revision_uid: Option<Uuid>,
    /// Current active plan revision.
    pub plan_revision: u64,
    /// Number of materialized logical tasks.
    pub total_tasks: u64,
    /// Number of completed logical tasks.
    pub completed_tasks: u64,
    /// Number of failed logical tasks.
    pub failed_tasks: u64,
    /// Current persisted budget ledger.
    pub budget_ledger: BudgetLedger,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Database-owned timestamp when the run first became queued.
    pub queued_at: Option<DateTime<Utc>>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// Terminal timestamp, when terminal.
    pub completed_at: Option<DateTime<Utc>>,
    /// Immutable typed terminal cause and requirement counts, when terminal.
    pub terminal_evidence: Option<ExecutionTerminalEvidence>,
    /// Exact normalized terminal reason, when terminal.
    pub terminal_reason: Option<ExecutionTerminalReason>,
}

/// Response from `Execution/start`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStartResponse {
    /// Current durable run projection.
    pub run: ExecutionRunSummary,
    /// Whether this call created the run rather than replaying an idempotency key.
    pub created: bool,
    /// Whether server policy requires explicit plan confirmation.
    pub confirmation_required: bool,
    /// Exact active-plan hash committed by start or idempotent replay.
    pub active_plan_hash: ExecutionHash,
    /// Exact retry- and fan-out-inclusive compiler estimate.
    pub estimate: crate::capability::ExecutionEstimate,
}

/// Response from `Execution/status`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStatusResponse {
    /// Current durable run projection.
    pub run: ExecutionRunSummary,
    /// Exact persisted scheduler wait reasons.
    pub waiting: Vec<WaitingReason>,
    /// Terminal structured output, when present.
    pub output: Option<Value>,
    /// Stable terminal completion gaps.
    pub gaps: Vec<String>,
}

/// Typed terminal delivery from the durable run workflow to the owning session.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTerminalDelivery {
    /// Exact persisted terminal run status.
    pub status: ExecutionRunStatus,
    /// Compact bounded terminal evidence.
    pub summary: ExecutionTerminalSummary,
}

/// Internal request to load immutable run evidence for a linked synthesis turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSynthesisEvidenceRequest {
    /// Parent-authorized run identity.
    pub run: ExecutionRunRequest,
    /// Exact persisted user event that originated the run.
    pub originating_user_sequence_num: u64,
}

/// Immutable goal and compact completion evidence injected only into synthesis context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSynthesisEvidence {
    /// Immutable user-derived goal contract.
    pub goal: moa_artifacts::execution_plan::ExecutionGoalContract,
    /// Persisted compact completion-check results.
    pub completion_check_results: Vec<Value>,
}

/// Builds compact aggregate progress from one canonical execution-run row.
pub fn execution_progress_from_run(
    run: &crate::repository::ExecutionRunRecord,
) -> Result<ExecutionProgress> {
    let phase = execution_progress_phase(run);
    let remaining = BudgetLedger {
        limit: run.approved_budget.clone(),
        reserved: run.reserved,
        consumed: run.consumed,
        overrun: run.budget_overrun,
    }
    .remaining_limit()?;
    Ok(ExecutionProgress {
        run_uid: run.run_uid,
        originating_user_sequence_num: run.originating_user_sequence_num,
        plan_revision: run.plan_revision,
        status: run.status.as_str().to_string(),
        phase,
        waiting_since: run.waiting_since,
        next_wake_at: run.next_wake_at,
        last_progress_at: run.last_progress_at,
        // A run can own multiple concurrent external jobs. Only a task-qualified
        // transition can name one exact job; the aggregate run row cannot.
        external_job_uid: None,
        ready_tasks: run.ready_task_count,
        active_tasks: run.active_task_count,
        parked_tasks: run.waiting_task_count,
        blocker_audience: execution_blocker_audience(run),
        remaining_budget: ExecutionRemainingBudget {
            cost_microusd: remaining.max_cost_microusd,
            tokens: remaining.max_tokens,
            tasks: remaining.max_tasks,
            tool_calls: remaining.max_tool_calls,
            retrieved_bytes: remaining.max_retrieved_bytes,
            deadline_at: remaining.deadline_at,
        },
        economics: Some(execution_progress_economics(
            &run.consumed,
            run.goal.requirements.len(),
            run.terminal_evidence.as_ref(),
        )?),
        total: run.progress_total_tasks,
        completed: run.progress_completed_tasks,
        failed: run.progress_failed_tasks,
        cancelled: run.progress_cancelled_tasks,
    })
}

/// Relates reconciled spend to the goal-requirement denominator for one progress event.
///
/// Every input is already resident on the loaded run row, so the projection adds no query
/// to the progress hot path. `requirements_satisfied` is durable only in terminal evidence,
/// so it stays `None` for the whole active life of the run.
fn execution_progress_economics(
    consumed: &ExecutionEstimate,
    requirement_count: usize,
    terminal_evidence: Option<&ExecutionTerminalEvidence>,
) -> Result<ExecutionProgressEconomics> {
    Ok(ExecutionProgressEconomics {
        consumed_cost_microusd: consumed.cost_microusd,
        consumed_tokens: consumed.tokens,
        consumed_tasks: consumed.tasks,
        consumed_tool_calls: consumed.tool_calls,
        consumed_retrieved_bytes: consumed.retrieved_bytes,
        requirements_total: u64::try_from(requirement_count).map_err(|_| {
            Error::ArithmeticOverflow {
                context: "execution progress requirement count".to_string(),
            }
        })?,
        requirements_satisfied: terminal_evidence
            .map(|evidence| evidence.satisfied_requirement_count),
    })
}

fn execution_blocker_audience(
    run: &crate::repository::ExecutionRunRecord,
) -> Option<ExecutionBlockerAudience> {
    execution_blocker_audience_from_flags(
        run.waiting_input_user_task_count > 0,
        run.waiting_input_tenant_admin_task_count > 0 || run.waiting_review_task_count > 0,
        run.waiting_input_external_task_count > 0
            || run.waiting_signal_task_count > 0
            || run.waiting_external_task_count > 0,
        run.waiting_timer_task_count > 0 || run.waiting_replan_task_count > 0,
    )
}

fn execution_blocker_audience_from_flags(
    user: bool,
    tenant_reviewer: bool,
    external: bool,
    system: bool,
) -> Option<ExecutionBlockerAudience> {
    if user {
        Some(ExecutionBlockerAudience::User)
    } else if tenant_reviewer {
        Some(ExecutionBlockerAudience::TenantReviewer)
    } else if external {
        Some(ExecutionBlockerAudience::External)
    } else if system {
        Some(ExecutionBlockerAudience::System)
    } else {
        None
    }
}

fn execution_progress_phase(run: &crate::repository::ExecutionRunRecord) -> ExecutionProgressPhase {
    execution_progress_phase_from_flags(
        run.status,
        run.waiting_input_task_count > 0,
        run.waiting_review_task_count > 0,
        run.waiting_signal_task_count > 0,
        run.waiting_timer_task_count > 0,
        run.waiting_external_task_count > 0,
    )
}

fn execution_progress_phase_from_flags(
    status: ExecutionRunStatus,
    waiting_input: bool,
    waiting_review: bool,
    waiting_signal: bool,
    waiting_timer: bool,
    waiting_external: bool,
) -> ExecutionProgressPhase {
    match status {
        ExecutionRunStatus::PauseRequested => ExecutionProgressPhase::PauseRequested,
        ExecutionRunStatus::Pausing => ExecutionProgressPhase::Pausing,
        ExecutionRunStatus::Paused => ExecutionProgressPhase::Paused,
        _ if waiting_input => ExecutionProgressPhase::WaitingInput,
        _ if waiting_review => ExecutionProgressPhase::WaitingReview,
        _ if waiting_signal => ExecutionProgressPhase::WaitingSignal,
        _ if waiting_timer => ExecutionProgressPhase::WaitingTimer,
        _ if waiting_external => ExecutionProgressPhase::WaitingExternal,
        _ => ExecutionProgressPhase::Running,
    }
}

/// Maps a non-success terminal run status to the session failure disposition.
pub fn execution_failure_disposition(
    status: ExecutionRunStatus,
) -> Result<ExecutionFailureDisposition> {
    match status {
        ExecutionRunStatus::Partial => Ok(ExecutionFailureDisposition::Partial),
        ExecutionRunStatus::Blocked => Ok(ExecutionFailureDisposition::Blocked),
        ExecutionRunStatus::Unsupported => Ok(ExecutionFailureDisposition::Unsupported),
        ExecutionRunStatus::Failed => Ok(ExecutionFailureDisposition::Failed),
        _ => Err(Error::InvalidRepositoryData {
            message: format!(
                "execution status `{}` has no failure disposition",
                status.as_str()
            ),
        }),
    }
}

/// Builds the bounded terminal summary stored on session events.
pub fn build_execution_terminal_summary(
    run_uid: Uuid,
    originating_user_sequence_num: u64,
    output: Option<&Value>,
    citation_ids: impl IntoIterator<Item = String>,
    failures: impl IntoIterator<Item = String>,
    gaps: impl IntoIterator<Item = String>,
) -> Result<ExecutionTerminalSummary> {
    let complete_output = output.cloned().unwrap_or(Value::Null);
    let canonical_output = moa_core::canonical_json::canonical_json_bytes(&complete_output)?;
    let output_hash = *blake3::hash(&canonical_output).as_bytes();
    let output = (canonical_output.len() <= EXECUTION_TERMINAL_INLINE_OUTPUT_MAX_BYTES)
        .then_some(complete_output)
        .filter(|_| output.is_some());

    let mut citation_ids = citation_ids.into_iter().collect::<Vec<_>>();
    if let Some(source_id) = citation_ids
        .iter()
        .find(|source_id| source_id.chars().count() > 512)
    {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "persisted citation source_id exceeds 512 characters: {} characters",
                source_id.chars().count()
            ),
        });
    }
    citation_ids.sort();
    citation_ids.dedup();
    citation_ids.truncate(EXECUTION_TERMINAL_MAX_CITATION_IDS);

    let failures = bounded_terminal_texts(failures, EXECUTION_TERMINAL_MAX_FAILURES);
    let gaps = bounded_terminal_texts(gaps, EXECUTION_TERMINAL_MAX_GAPS);
    Ok(ExecutionTerminalSummary {
        run_uid,
        originating_user_sequence_num,
        output,
        output_hash,
        citation_ids,
        failures,
        gaps,
        task_results: ExecutionTaskResultsRef::ExecutionTaskTable { run_uid },
    })
}

/// Builds one typed terminal delivery exclusively from a durable run and task projection.
pub fn execution_terminal_delivery_from_state(
    run: &crate::repository::ExecutionRunRecord,
    projection: &ExecutionProjection,
) -> Result<ExecutionTerminalDelivery> {
    if !run.status.is_terminal() {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "execution run `{}` is not terminal at status `{}`",
                run.run_uid,
                run.status.as_str()
            ),
        });
    }
    if projection.plan_revision != run.plan_revision {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "execution terminal projection revision {} differs from persisted revision {}",
                projection.plan_revision, run.plan_revision
            ),
        });
    }

    let mut tasks = projection.tasks.iter().collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        (&left.node_id, &left.item_key, left.task_id).cmp(&(
            &right.node_id,
            &right.item_key,
            right.task_id,
        ))
    });
    let mut citation_ids = Vec::new();
    let mut failures = Vec::new();
    for task in tasks {
        let Some(outcome) = task.outcome.as_ref() else {
            continue;
        };
        match &outcome.result {
            ExecutionTaskResult::Completed { citations, .. } => {
                citation_ids.extend(citations.iter().map(|citation| citation.source_id.clone()))
            }
            ExecutionTaskResult::Failed { message, .. } => failures.push(message.clone()),
            ExecutionTaskResult::UnknownOutcome { message } => failures.push(message.clone()),
            ExecutionTaskResult::NeedsInput { .. }
            | ExecutionTaskResult::NeedsReplan { .. }
            | ExecutionTaskResult::Cancelled { .. } => {}
        }
    }

    let summary = build_execution_terminal_summary(
        run.run_uid,
        run.originating_user_sequence_num,
        run.output.as_ref(),
        citation_ids,
        failures,
        run.terminal_gaps.clone(),
    )?;
    Ok(ExecutionTerminalDelivery {
        status: run.status,
        summary,
    })
}

fn bounded_terminal_texts(values: impl IntoIterator<Item = String>, limit: usize) -> Vec<String> {
    values
        .into_iter()
        .take(limit)
        .map(|value| {
            value
                .chars()
                .take(EXECUTION_TERMINAL_TEXT_MAX_CHARS)
                .collect()
        })
        .collect()
}

/// Bounded run-list response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunListResponse {
    /// Current page of run summaries.
    pub runs: Vec<ExecutionRunSummary>,
    /// Opaque keyset cursor for the next page.
    pub next_cursor: Option<String>,
}

/// Bounded task-result response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskListResponse {
    /// Current page of task projections.
    pub tasks: Vec<ExecutionTaskProjection>,
    /// Opaque keyset cursor for the next page.
    pub next_cursor: Option<String>,
}

/// Stable reason a mutation changed no durable state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionConflictReason {
    /// Persisted tenant/contact/session ownership differed from the request.
    ScopeMismatch,
    /// Current run or task status did not permit the transition.
    InvalidStatus,
    /// Task generation fence did not match.
    GenerationMismatch,
    /// Active plan revision fence did not match.
    PlanRevisionMismatch,
    /// Active plan hash differed from the displayed hash.
    PlanHashMismatch,
    /// Approved budget differed from an exact confirmation replay.
    BudgetMismatch,
    /// Waiting task declared a different input audience.
    AudienceMismatch,
    /// Waiting task declared a different signal name.
    SignalMismatch,
    /// The run or task was already terminal.
    AlreadyTerminal,
}

/// Common response for execution mutations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionMutationResponse {
    /// The mutation changed durable state.
    Applied {
        /// Current durable run projection.
        run: ExecutionRunSummary,
    },
    /// The same mutation was already durably applied.
    Replayed {
        /// Current durable run projection.
        run: ExecutionRunSummary,
    },
    /// A stable compare-and-set or scope conflict changed nothing.
    Conflict {
        /// Stable conflict reason.
        reason: ExecutionConflictReason,
    },
    /// No scoped run or task exists.
    NotFound,
}

/// Typed terminal action-policy review delivery to an execution task.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionActionReviewResolutionRequest {
    /// Owning run.
    pub run_uid: Uuid,
    /// Owning task.
    pub task_id: ExecutionTaskId,
    /// Task generation fenced by the review.
    pub generation: u64,
    /// Stable action-review identifier and idempotency key.
    pub review_uid: Uuid,
    /// Typed terminal resolution.
    pub resolution: ExecutionActionReviewResolution,
}

/// Stable reason an execution-scoped tool effect was not dispatched.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionToolDispatchRejection {
    /// The scoped run or operation does not exist for the owning session.
    OriginNotFound,
    /// The request carries an obsolete task or compensation generation.
    StaleGeneration,
    /// The operation is no longer in its dispatchable running state.
    OperationNotRunning,
    /// The run is terminal, fenced, compensating incorrectly, or awaiting manual repair.
    RunNotDispatchable,
}

/// Terminal outcome delivered by the action-review outbox.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionActionReviewResolution {
    /// The approved tool completed and returned structured output.
    Completed {
        /// Serialized governed tool output.
        tool_output: Value,
    },
    /// The approved capability committed asynchronous provider work.
    ExternalJob {
        /// MOA-owned job identity reserved before provider dispatch.
        external_job_uid: Uuid,
        /// Immutable provider job identity and recovery contract.
        job: AsyncToolJob,
    },
    /// The approved tool failed during dispatch.
    Failed {
        /// Typed task failure classification.
        class: ExecutionFailureClass,
        /// Human-readable failure message.
        message: String,
    },
    /// The reviewed effect may have committed, but no authoritative result was recovered.
    UnknownOutcome {
        /// Stable diagnostic describing why the effect remains ambiguous.
        message: String,
    },
    /// The reviewed effect was definitively fenced before dispatch.
    NotDispatched {
        /// Closed reason the effect was never started.
        reason: ExecutionToolDispatchRejection,
    },
    /// Tenant policy denied the action.
    Denied {
        /// Denial reason.
        reason: String,
    },
    /// The review expired before a decision.
    TimedOut {
        /// Timeout reason.
        reason: String,
    },
}

/// Typed terminal action-policy review delivery to a compensation generation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCompensationReviewResolutionRequest {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable compensation workflow identity.
    pub compensation_id: CompensationId,
    /// Compensation generation fenced by the review.
    pub generation: u64,
    /// Stable action-review identifier and idempotency key.
    pub review_uid: Uuid,
    /// Typed terminal resolution produced by governed action dispatch.
    pub resolution: ExecutionActionReviewResolution,
}

/// Immutable request that executes one bounded task-attempt slice.
///
/// The Restate workflow key is [`Self::dispatch_uid`]. Re-delivery of this
/// request can replay the same slice, but a different dispatch UID must never
/// continue or replace it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskAttemptRequest {
    /// Immutable durable-dispatch identity and workflow key.
    pub dispatch_uid: Uuid,
    /// Exact active-capacity receipt released when this slice yields or settles.
    pub capacity_reservation_uid: Uuid,
    /// Exact watchdog trigger owned only while this slice is active.
    pub watchdog_trigger_uid: Uuid,
    /// Durable delayed-delivery dispatch for the exact watchdog trigger.
    pub watchdog_dispatch_uid: Uuid,
    /// Owning run.
    pub run_uid: Uuid,
    /// Stable logical task.
    pub task_id: ExecutionTaskId,
    /// Run-controller generation that admitted the slice.
    pub controller_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Absolute deadline committed by admission.
    pub attempt_deadline_at: DateTime<Utc>,
    /// Owning tenant.
    pub tenant_id: TenantId,
}

/// Delivery of the exact watchdog owned by one active task attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskAttemptWatchdogRequest {
    /// Immutable durable-dispatch identity and workflow key.
    pub dispatch_uid: Uuid,
    /// Exact active-capacity receipt released by watchdog settlement.
    pub capacity_reservation_uid: Uuid,
    /// Trigger whose delivery caused this check.
    pub watchdog_trigger_uid: Uuid,
    /// Owning run.
    pub run_uid: Uuid,
    /// Stable logical task.
    pub task_id: ExecutionTaskId,
    /// Run-controller generation that admitted the slice.
    pub controller_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Owning tenant.
    pub tenant_id: TenantId,
}

/// Immutable request that executes one bounded compensation-attempt slice.
///
/// The Restate workflow key is [`Self::dispatch_uid`]. Logical compensation
/// generation and attempt generation are distinct fences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCompensationAttemptRequest {
    /// Immutable durable-dispatch identity and workflow key.
    pub dispatch_uid: Uuid,
    /// Exact shared active-capacity receipt released when this slice returns.
    pub capacity_reservation_uid: Uuid,
    /// Exact watchdog trigger owned only while this slice is active.
    pub watchdog_trigger_uid: Uuid,
    /// Durable delayed-delivery dispatch for the exact watchdog trigger.
    pub watchdog_dispatch_uid: Uuid,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable compensation registration.
    pub compensation_id: CompensationId,
    /// Logical compensation generation selected in strict reverse order.
    pub compensation_generation: u64,
    /// Exact bounded compensation-attempt generation.
    pub compensation_attempt_generation: u64,
    /// Run-controller generation that admitted the slice.
    pub controller_generation: u64,
    /// Absolute deadline committed by admission.
    pub attempt_deadline_at: DateTime<Utc>,
    /// Owning tenant.
    pub tenant_id: TenantId,
}

/// Delivery of the exact watchdog owned by one active compensation attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCompensationAttemptWatchdogRequest {
    /// Immutable durable-dispatch identity and workflow key.
    pub dispatch_uid: Uuid,
    /// Exact shared active-capacity receipt released by watchdog settlement.
    pub capacity_reservation_uid: Uuid,
    /// Trigger whose delivery caused this check.
    pub watchdog_trigger_uid: Uuid,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable compensation registration.
    pub compensation_id: CompensationId,
    /// Logical compensation generation selected in strict reverse order.
    pub compensation_generation: u64,
    /// Exact bounded compensation-attempt generation.
    pub compensation_attempt_generation: u64,
    /// Run-controller generation that admitted the slice.
    pub controller_generation: u64,
    /// Owning tenant.
    pub tenant_id: TenantId,
}

/// Durable reason one exact active attempt must checkpoint and relinquish ownership.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAttemptCancelReason {
    /// The run's immutable approved deadline elapsed.
    DeadlineExceeded,
    /// Another terminal intent fenced all remaining forward work.
    RunTerminal,
    /// An authorized pause fenced new work and is draining active slices.
    PauseRequested,
    /// Provider-owned asynchronous work was committed before the slice relinquished compute.
    ExternalJobStarted,
}

/// Durable reason one compensation slice relinquishes its active sandbox ownership.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCompensationReleaseIntent {
    /// A definitive compensation outcome is ready to settle.
    Outcome,
    /// A retryable compensation failure is ready to requeue.
    Retry,
    /// The governed action is parking for tenant review.
    Review,
    /// Provider-owned asynchronous work was durably started.
    ExternalJob,
    /// An authorized pause is draining the active slice.
    Pause,
    /// The exact attempt watchdog elapsed.
    Watchdog,
    /// The immutable run deadline elapsed.
    Deadline,
    /// Another terminal run intent fenced the active slice.
    RunTerminal,
}

/// Identity-free cancellation delivery for one exact active task attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskAttemptCancelRequest {
    /// Immutable outbox delivery identity and Restate idempotency key.
    #[serde(rename = "dispatch_uid")]
    pub cancellation_dispatch_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable logical task.
    pub task_id: ExecutionTaskId,
    /// Exact controller generation that fenced the attempt.
    pub controller_generation: u64,
    /// Controller generation carried by the immutable attempt resources being released.
    pub attempt_controller_generation: u64,
    /// Exact logical task generation.
    pub task_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Immutable dispatch identity that owns the active slice.
    pub active_dispatch_uid: Uuid,
    /// Exact active-capacity receipt released only after sandbox ownership is relinquished.
    pub capacity_reservation_uid: Uuid,
    /// Exact watchdog superseded by cancellation settlement.
    pub watchdog_trigger_uid: Uuid,
    /// Closed reason for the ownership transfer.
    pub reason: ExecutionAttemptCancelReason,
}

/// Identity-free cancellation delivery for one exact compensation attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCompensationAttemptCancelRequest {
    /// Immutable outbox delivery identity and Restate idempotency key.
    #[serde(rename = "dispatch_uid")]
    pub cancellation_dispatch_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable compensation registration.
    pub compensation_id: CompensationId,
    /// Exact controller generation that fenced the attempt.
    pub controller_generation: u64,
    /// Controller generation carried by the immutable attempt resources being released.
    pub attempt_controller_generation: u64,
    /// Exact logical compensation generation.
    pub compensation_generation: u64,
    /// Exact bounded compensation-attempt generation.
    pub compensation_attempt_generation: u64,
    /// Immutable dispatch identity that owns the active slice.
    pub active_dispatch_uid: Uuid,
    /// Exact active-capacity receipt released only after sandbox ownership is relinquished.
    pub capacity_reservation_uid: Uuid,
    /// Exact watchdog superseded by cancellation settlement.
    pub watchdog_trigger_uid: Uuid,
    /// Closed reason for the compensation ownership transfer.
    pub intent: ExecutionCompensationReleaseIntent,
}

/// Immutable request to cancel one exact asynchronous provider-job generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionExternalJobCancelRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Expected provider implementation name.
    pub provider: String,
    /// Expected provider-issued job identity.
    pub provider_job_id: String,
    /// Stable provider idempotency key reused for cancellation.
    pub idempotency_key: String,
}

/// Exact task or compensation owner duplicated by a start-recovery trigger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "owner_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionExternalJobStartRecoveryOwner {
    /// One forward-task attempt.
    Task {
        /// Stable logical task identity.
        task_id: Uuid,
        /// Exact task-attempt generation.
        attempt_generation: u64,
    },
    /// One compensation attempt.
    Compensation {
        /// Stable compensation identity.
        compensation_id: Uuid,
        /// Exact compensation logical generation.
        compensation_generation: u64,
        /// Exact compensation-attempt generation.
        compensation_attempt_generation: u64,
    },
}

/// Durable delivery request for crash-safe provider start recovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionExternalJobStartRecoveryRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Exact task or compensation owner.
    pub owner: ExecutionExternalJobStartRecoveryOwner,
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Declared adapter/provider key reserved before dispatch.
    pub provider: String,
    /// Stable provider start idempotency key.
    pub idempotency_key: String,
    /// Exact temporal trigger being delivered.
    pub trigger_uid: Uuid,
}

/// Durable result of one provider start-recovery delivery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionExternalJobStartRecoveryResponse {
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Generation-fenced recovery disposition.
    pub outcome: ExecutionExternalJobStartRecoveryResponseOutcome,
}

/// Typed acknowledgement from a bounded task or compensation watchdog receiver.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAttemptWatchdogResponse {
    /// Whether trigger delivery may settle or must retry.
    pub outcome: ExecutionAttemptWatchdogResponseOutcome,
}

/// Durable watchdog receiver disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAttemptWatchdogResponseOutcome {
    /// The exact active attempt and capacity were settled by this invocation.
    Settled,
    /// Exact work was already settled or became stale before this replay.
    ReplayedOrStale,
    /// The receiver could not safely settle; trigger delivery must remain retryable.
    RetryDelivery,
}

/// Result of recovering one pre-reserved provider start.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionExternalJobStartRecoveryResponseOutcome {
    /// Provider proved that no work started and the intent was released.
    NotStartedReleased,
    /// Provider start was found and exact ownership was bound.
    StartedBound,
    /// Provider outcome remains ambiguous and recovery work was rearmed.
    UnknownPreserved,
    /// The delivery no longer names the current unbound intent.
    StaleDelivery,
    /// The intent was already bound or released by another delivery.
    AlreadySettled,
}

/// Durable result of one bounded provider cancellation invocation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionExternalJobCancelResponse {
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Typed settlement result, including generation-fenced no-op deliveries.
    pub outcome: ExecutionExternalJobCancelResponseOutcome,
}

/// Result of one generation-fenced external-job cancellation delivery.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionExternalJobCancelResponseOutcome {
    /// The exact current provider job was called and its result was persisted.
    Applied {
        /// Typed provider cancellation result.
        provider_outcome: AsyncToolJobCancelOutcome,
    },
    /// The delivery no longer names the current job generation or provider identity.
    StaleDelivery,
    /// The job had already reached a terminal state before this delivery.
    AlreadyTerminal,
    /// No visible job has the supplied MOA identity.
    NotFound,
}

/// Immutable request to reconcile one exact asynchronous provider-job generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionExternalJobReconcileRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact durable reconcile-trigger identity used as the synthetic provider event fence.
    pub trigger_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Expected provider implementation name.
    pub provider: String,
    /// Expected provider-issued job identity.
    pub provider_job_id: String,
    /// Stable provider idempotency key reused for reconciliation.
    pub idempotency_key: String,
}

/// Typed result of one bounded sparse provider reconciliation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionExternalJobReconcileResponse {
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Generation-fenced durable reconciliation disposition.
    pub outcome: ExecutionExternalJobReconcileResponseOutcome,
}

/// Result of one generation-fenced sparse provider reconciliation delivery.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionExternalJobReconcileResponseOutcome {
    /// The exact provider job was observed and its result was persisted.
    Applied {
        /// Typed progress or terminal provider observation.
        provider_outcome: AsyncToolJobCallbackOutcome,
    },
    /// The delivery no longer names the current job generation or provider identity.
    StaleDelivery,
    /// The job had already reached a terminal state before this delivery.
    AlreadyTerminal,
    /// No visible job has the supplied MOA identity.
    NotFound,
}

/// Encodes a cursor as canonical JSON in URL-safe unpadded base64.
pub fn encode_cursor<T: Serialize + ?Sized>(cursor: &T) -> Result<String> {
    let bytes = moa_core::canonical_json::canonical_json_bytes(cursor)?;
    Ok(format!("{CURSOR_PREFIX}{}", encode_base64_url(&bytes)))
}

/// Decodes and strictly deserializes one URL-safe cursor.
pub fn decode_cursor<T: DeserializeOwned>(cursor: &str) -> Result<T> {
    let encoded = cursor
        .strip_prefix(CURSOR_PREFIX)
        .ok_or_else(|| invalid_cursor("missing cursor prefix"))?;
    let bytes = decode_base64_url(encoded)?;
    serde_json::from_slice(&bytes).map_err(|error| invalid_cursor(&error.to_string()))
}

fn encode_base64_url(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(char::from(BASE64_URL_ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            BASE64_URL_ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                BASE64_URL_ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        }
        if chunk.len() > 2 {
            encoded.push(char::from(BASE64_URL_ALPHABET[usize::from(third & 0x3f)]));
        }
    }
    encoded
}

fn decode_base64_url(encoded: &str) -> Result<Vec<u8>> {
    if encoded.is_empty() || encoded.len() % 4 == 1 || encoded.contains('=') {
        return Err(invalid_cursor("invalid URL-safe base64 length or padding"));
    }
    let values = encoded
        .bytes()
        .map(base64_value)
        .collect::<Result<Vec<_>>>()?;
    let mut decoded = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        decoded.push((chunk[0] << 2) | (chunk[1] >> 4));
        if chunk.len() > 2 {
            decoded.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() > 3 {
            decoded.push((chunk[2] << 6) | chunk[3]);
        }
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(invalid_cursor("invalid URL-safe base64 character")),
    }
}

fn invalid_cursor(message: &str) -> Error {
    Error::InvalidRepositoryInput {
        message: format!("invalid execution cursor: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_progress_economics_reports_consumed_spend_against_requirements_offline() {
        // Pins: progress projects reconciled spend and the goal-requirement denominator from
        // the already-loaded run row, and only claims satisfied requirements once terminal
        // evidence exists, so a mid-run reader is never told a stuck run satisfied anything.
        let consumed = ExecutionEstimate {
            cost_microusd: 12_500,
            tokens: 1_400_000,
            tool_calls: 31,
            retrieved_bytes: 640_000,
            tasks: 9,
        };

        let active =
            execution_progress_economics(&consumed, 4, None).expect("project active economics");
        assert_eq!(active.consumed_cost_microusd, 12_500);
        assert_eq!(active.consumed_tokens, 1_400_000);
        assert_eq!(active.consumed_tasks, 9);
        assert_eq!(active.consumed_tool_calls, 31);
        assert_eq!(active.consumed_retrieved_bytes, 640_000);
        assert_eq!(active.requirements_total, 4);
        assert_eq!(active.requirements_satisfied, None);

        let evidence = ExecutionTerminalEvidence {
            cause: crate::state::ExecutionTerminalCause::Completion { limit_stop: None },
            satisfied_requirement_count: 3,
            requirement_count: 4,
        };
        let terminal = execution_progress_economics(&consumed, 4, Some(&evidence))
            .expect("project terminal economics");
        assert_eq!(terminal.requirements_satisfied, Some(3));
        assert_eq!(terminal.requirements_total, 4);
        assert_eq!(
            terminal.consumed_cost_microusd, active.consumed_cost_microusd,
            "terminal evaluation must not restate spend"
        );
    }

    #[test]
    fn execution_progress_phase_exhaustively_maps_aggregate_wait_and_pause_states_offline() {
        // Pins: run-only progress distinguishes every public storage-only wait and pause phase;
        // task-only stale work has no aggregate run-status mapping.
        let expected = [
            (
                ExecutionRunStatus::WaitingInput,
                ExecutionProgressPhase::WaitingInput,
                [true, false, false, false, false],
            ),
            (
                ExecutionRunStatus::WaitingReview,
                ExecutionProgressPhase::WaitingReview,
                [false, true, false, false, false],
            ),
            (
                ExecutionRunStatus::WaitingSignal,
                ExecutionProgressPhase::WaitingSignal,
                [false, false, true, false, false],
            ),
            (
                ExecutionRunStatus::WaitingTimer,
                ExecutionProgressPhase::WaitingTimer,
                [false, false, false, true, false],
            ),
            (
                ExecutionRunStatus::WaitingExternal,
                ExecutionProgressPhase::WaitingExternal,
                [false, false, false, false, true],
            ),
            (
                ExecutionRunStatus::PauseRequested,
                ExecutionProgressPhase::PauseRequested,
                [false; 5],
            ),
            (
                ExecutionRunStatus::Pausing,
                ExecutionProgressPhase::Pausing,
                [false; 5],
            ),
            (
                ExecutionRunStatus::Paused,
                ExecutionProgressPhase::Paused,
                [false; 5],
            ),
        ];
        for (status, phase, waits) in expected {
            assert_eq!(
                execution_progress_phase_from_flags(
                    status, waits[0], waits[1], waits[2], waits[3], waits[4]
                ),
                phase,
                "status {status:?}"
            );
        }

        let aggregate_running = [
            ExecutionRunStatus::AwaitingConfirmation,
            ExecutionRunStatus::Queued,
            ExecutionRunStatus::Running,
            ExecutionRunStatus::WaitingReplan,
            ExecutionRunStatus::Compensating,
            ExecutionRunStatus::Completed,
            ExecutionRunStatus::Partial,
            ExecutionRunStatus::Blocked,
            ExecutionRunStatus::Unsupported,
            ExecutionRunStatus::Failed,
            ExecutionRunStatus::Cancelled,
        ];
        for status in aggregate_running {
            assert_eq!(
                execution_progress_phase_from_flags(status, false, false, false, false, false),
                ExecutionProgressPhase::Running,
                "status {status:?}"
            );
        }
    }

    #[test]
    fn execution_blocker_audience_uses_exact_scalar_priority_not_reason_samples_offline() {
        // Pins: truncated display samples cannot hide a higher-priority exact blocker; scalar
        // audience counters always order User > TenantReviewer > External > Agent > System.
        assert_eq!(
            execution_blocker_audience_from_flags(true, true, true, true),
            Some(ExecutionBlockerAudience::User)
        );
        assert_eq!(
            execution_blocker_audience_from_flags(false, true, true, true),
            Some(ExecutionBlockerAudience::TenantReviewer)
        );
        assert_eq!(
            execution_blocker_audience_from_flags(false, false, true, true),
            Some(ExecutionBlockerAudience::External)
        );
        assert_eq!(
            execution_blocker_audience_from_flags(false, false, false, true),
            Some(ExecutionBlockerAudience::System)
        );
        assert_eq!(
            execution_blocker_audience_from_flags(false, false, false, false),
            None
        );
    }

    // Pins: cancel outbox payloads expose the SQL-validated `dispatch_uid` key while
    // keeping the Rust field explicit about its cancellation-delivery ownership.
    #[test]
    fn attempt_cancel_payload_uses_dispatch_uid_wire_key_offline() {
        let request = ExecutionTaskAttemptCancelRequest {
            cancellation_dispatch_uid: Uuid::from_u128(1),
            tenant_id: TenantId::from(Uuid::from_u128(2)),
            run_uid: Uuid::from_u128(3),
            task_id: ExecutionTaskId::from_uuid(Uuid::from_u128(4)),
            controller_generation: 5,
            attempt_controller_generation: 4,
            task_generation: 6,
            attempt_generation: 7,
            active_dispatch_uid: Uuid::from_u128(8),
            capacity_reservation_uid: Uuid::from_u128(9),
            watchdog_trigger_uid: Uuid::from_u128(10),
            reason: ExecutionAttemptCancelReason::DeadlineExceeded,
        };

        let value = serde_json::to_value(request).expect("serialize task cancel request");
        assert_eq!(
            value.get("dispatch_uid"),
            Some(&serde_json::json!(Uuid::from_u128(1)))
        );
        assert!(value.get("cancellation_dispatch_uid").is_none());
    }

    #[test]
    fn cursor_round_trip_is_url_safe_and_strict() {
        // Pins: public cursors are canonical URL-safe base64 and malformed data is rejected.
        let cursor = ExecutionTaskCursor {
            node_id: "screen/company".to_string(),
            item_key: "A+B/C=".to_string(),
            task_id: ExecutionTaskId::from_uuid(Uuid::nil()),
        };

        let encoded = encode_cursor(&cursor).expect("cursor should encode");
        assert!(encoded.starts_with("cursor:"));
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
        assert_eq!(
            decode_cursor::<ExecutionTaskCursor>(&encoded).expect("cursor should decode"),
            cursor
        );
        assert!(decode_cursor::<ExecutionTaskCursor>("legacy:e30").is_err());
        assert!(decode_cursor::<ExecutionTaskCursor>("cursor:***").is_err());
    }

    #[test]
    fn execution_run_terminal_summary_enforces_all_compact_bounds() {
        // Pins: session delivery is bounded independently of execution-task table size.
        let run_uid = Uuid::from_u128(9);
        let summary = build_execution_terminal_summary(
            run_uid,
            17,
            Some(&serde_json::json!({ "answer": 42 })),
            (0..105)
                .rev()
                .map(|index| format!("source-{index:03}"))
                .chain(std::iter::once("source-001".to_string())),
            (0..25).map(|index| format!("failure-{index}-{}", "x".repeat(600))),
            (0..55).map(|index| format!("gap-{index}-{}", "y".repeat(600))),
        )
        .expect("bounded terminal summary");

        assert_eq!(summary.run_uid, run_uid);
        assert_eq!(summary.originating_user_sequence_num, 17);
        assert_eq!(summary.citation_ids.len(), 100);
        assert!(
            summary
                .citation_ids
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_eq!(summary.failures.len(), 20);
        assert_eq!(summary.gaps.len(), 50);
        assert!(
            summary
                .failures
                .iter()
                .all(|value| value.chars().count() == 512)
        );
        assert!(
            summary
                .gaps
                .iter()
                .all(|value| value.chars().count() == 512)
        );
        assert_eq!(summary.output, Some(serde_json::json!({ "answer": 42 })));
        assert_eq!(
            summary.task_results,
            ExecutionTaskResultsRef::ExecutionTaskTable { run_uid }
        );
    }

    #[test]
    fn execution_run_terminal_summary_hashes_full_output_when_inline_is_omitted() {
        // Pins: oversized output remains recoverable by hash/table reference without event copying.
        let run_uid = Uuid::from_u128(10);
        let output = serde_json::json!({ "body": "x".repeat(17 * 1024) });
        let canonical = moa_core::canonical_json::canonical_json_bytes(&output)
            .expect("canonicalize expected full output");
        let summary = build_execution_terminal_summary(
            run_uid,
            18,
            Some(&output),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("oversized terminal summary");

        assert_eq!(summary.output, None);
        assert_eq!(summary.output_hash, *blake3::hash(&canonical).as_bytes());
    }

    #[test]
    fn execution_run_terminal_summary_inlines_exactly_sixteen_kibibytes() {
        // Pins: the inline decision uses canonical JSON bytes with an inclusive 16 KiB bound.
        let run_uid = Uuid::from_u128(11);
        for (string_bytes, expected_inline) in [(16 * 1024 - 2, true), (16 * 1024 - 1, false)] {
            let output = Value::String("x".repeat(string_bytes));
            let canonical = moa_core::canonical_json::canonical_json_bytes(&output)
                .expect("canonicalize boundary output");
            let summary = build_execution_terminal_summary(
                run_uid,
                19,
                Some(&output),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .expect("build boundary terminal summary");

            assert_eq!(canonical.len() <= 16 * 1024, expected_inline);
            assert_eq!(summary.output.as_ref() == Some(&output), expected_inline);
            assert_eq!(summary.output_hash, *blake3::hash(&canonical).as_bytes());
        }
    }

    #[test]
    fn execution_failure_disposition_is_closed_over_failure_terminals() {
        // Pins: only failed-delivery terminal states map to the four public dispositions.
        assert_eq!(
            execution_failure_disposition(ExecutionRunStatus::Partial)
                .expect("partial disposition"),
            ExecutionFailureDisposition::Partial
        );
        assert_eq!(
            execution_failure_disposition(ExecutionRunStatus::Blocked)
                .expect("blocked disposition"),
            ExecutionFailureDisposition::Blocked
        );
        assert_eq!(
            execution_failure_disposition(ExecutionRunStatus::Unsupported)
                .expect("unsupported disposition"),
            ExecutionFailureDisposition::Unsupported
        );
        assert_eq!(
            execution_failure_disposition(ExecutionRunStatus::Failed).expect("failed disposition"),
            ExecutionFailureDisposition::Failed
        );
        for status in [ExecutionRunStatus::Completed, ExecutionRunStatus::Cancelled] {
            assert!(
                execution_failure_disposition(status).is_err(),
                "{status:?} must use its dedicated terminal event"
            );
        }
    }

    #[test]
    fn execution_terminal_citation_limit_counts_unicode_scalars() {
        // Pins: citation IDs are bounded by Unicode scalar count, not UTF-8 byte length.
        let run_uid = Uuid::from_u128(12);
        let accepted = "é".repeat(512);
        let summary = build_execution_terminal_summary(
            run_uid,
            20,
            None,
            [accepted.clone()],
            Vec::new(),
            Vec::new(),
        )
        .expect("512 Unicode scalar citation ID must be accepted");
        assert_eq!(summary.citation_ids, vec![accepted]);

        let error = build_execution_terminal_summary(
            run_uid,
            20,
            None,
            ["é".repeat(513)],
            Vec::new(),
            Vec::new(),
        )
        .expect_err("513 Unicode scalar citation ID must be rejected");
        assert!(matches!(
            error,
            Error::InvalidRepositoryData { message }
                if message.contains("exceeds 512 characters: 513 characters")
        ));
    }

    #[test]
    fn execution_template_admission_identity_and_fingerprint_are_canonical() {
        // Pins: tenant/key framing and complete-request canonical JSON remain byte-stable.
        let request = ExecutionTemplateAdmissionRequest {
            tenant_id: TenantId::from(Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111)),
            contact_id: Some(ContactId(Uuid::from_u128(
                0x2222_2222_2222_2222_2222_2222_2222_2222,
            ))),
            session_id: SessionId(Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333)),
            template: PinnedExecutionTemplateRef {
                skill_ref: "skill://quarterly-research".to_string(),
                revision_uid: Uuid::from_u128(0x4444_4444_4444_4444_4444_4444_4444_4444),
            },
            objective: "Research this exact objective.".to_string(),
            input: serde_json::json!({"z": 1, "a": [true, null]}),
            idempotency_key: Some("Case-Sensitive-Key".to_string()),
        };

        assert_eq!(
            execution_template_admission_operation_uid(
                request.tenant_id,
                request
                    .idempotency_key
                    .as_deref()
                    .expect("fixture has idempotency key"),
            )
            .expect("derive operation UID")
            .to_string(),
            "c0b18db9-d980-547a-8573-139727f8e848"
        );
        assert_eq!(
            execution_template_admission_request_fingerprint(&request)
                .expect("fingerprint admission")
                .to_string(),
            "d47df13812a5a947579cb5e33bf05a7b6ad2496ccd7be68d75d17446ece6d444"
        );
    }

    #[test]
    fn execution_template_admission_fingerprint_binds_complete_request() {
        // Pins: one tenant key cannot replay with changed scope, template, objective, input, or key.
        let request = ExecutionTemplateAdmissionRequest {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            contact_id: Some(ContactId(Uuid::from_u128(2))),
            session_id: SessionId(Uuid::from_u128(3)),
            template: PinnedExecutionTemplateRef {
                skill_ref: "skill://archive".to_string(),
                revision_uid: Uuid::from_u128(4),
            },
            objective: "Preserve the archive".to_string(),
            input: serde_json::json!({"priority": 1}),
            idempotency_key: Some("archive-key".to_string()),
        };
        let first = execution_template_admission_request_fingerprint(&request)
            .expect("fingerprint complete request");
        let changed = [
            ExecutionTemplateAdmissionRequest {
                tenant_id: TenantId::from(Uuid::from_u128(10)),
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                contact_id: Some(ContactId(Uuid::from_u128(20))),
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                session_id: SessionId(Uuid::from_u128(30)),
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                template: PinnedExecutionTemplateRef {
                    skill_ref: "skill://replacement".to_string(),
                    revision_uid: request.template.revision_uid,
                },
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                template: PinnedExecutionTemplateRef {
                    revision_uid: Uuid::from_u128(40),
                    ..request.template.clone()
                },
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                objective: "Preserve a different archive".to_string(),
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                input: serde_json::json!({"priority": 2}),
                ..request.clone()
            },
            ExecutionTemplateAdmissionRequest {
                idempotency_key: Some("different-key".to_string()),
                ..request.clone()
            },
        ];

        for changed_request in changed {
            assert_ne!(
                execution_template_admission_request_fingerprint(&changed_request)
                    .expect("fingerprint changed request"),
                first,
                "every canonical request field must participate in the fingerprint"
            );
        }
    }

    #[test]
    fn execution_template_admission_rejects_nil_contact_and_invalid_key_without_normalizing() {
        // Pins: nil scope and empty/oversized keys fail before persistence; key bytes stay exact.
        let mut request = ExecutionTemplateAdmissionRequest {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            contact_id: Some(ContactId(Uuid::nil())),
            session_id: SessionId(Uuid::from_u128(2)),
            template: PinnedExecutionTemplateRef {
                skill_ref: "skill://archive".to_string(),
                revision_uid: Uuid::from_u128(3),
            },
            objective: String::new(),
            input: Value::Null,
            idempotency_key: Some("key".to_string()),
        };
        assert!(matches!(
            request.validate(),
            Err(Error::InvalidRepositoryInput { .. })
        ));

        request.contact_id = None;
        request.idempotency_key = Some(String::new());
        assert!(request.validate().is_err());
        request.idempotency_key = Some("x".repeat(257));
        assert!(request.validate().is_err());
        request.idempotency_key = Some("é".repeat(128));
        assert!(request.validate().is_ok());
        request.idempotency_key = Some("é".repeat(129));
        assert!(request.validate().is_err());

        let lower = execution_template_admission_operation_uid(request.tenant_id, "key")
            .expect("lowercase key UID");
        let upper = execution_template_admission_operation_uid(request.tenant_id, "Key")
            .expect("uppercase key UID");
        assert_ne!(lower, upper);
    }
}
