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
    ExecutionFailureDisposition, ExecutionProgress, ExecutionTaskResultsRef,
    ExecutionTerminalSummary,
};
use moa_core::traits::Identity;
use moa_core::types::{
    contact::ContactId,
    execution_planning::{ExecutionSourceProvenance, PinnedExecutionTemplateRef},
    identifiers::{SessionId, TenantId, UserId},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    Error, Result,
    budget::BudgetLedger,
    capability::{ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionHash},
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
#[must_use]
pub fn execution_progress_from_run(
    run: &crate::repository::ExecutionRunRecord,
) -> ExecutionProgress {
    ExecutionProgress {
        run_uid: run.run_uid,
        originating_user_sequence_num: run.originating_user_sequence_num,
        plan_revision: run.plan_revision,
        status: run.status.as_str().to_string(),
        total: run.progress_total_tasks,
        completed: run.progress_completed_tasks,
        failed: run.progress_failed_tasks,
        cancelled: run.progress_cancelled_tasks,
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

/// Idempotent acknowledgement returned to the action-review outbox dispatcher.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionActionReviewAcknowledgement {
    /// The resolution was applied to the current task generation.
    Applied,
    /// This review UID was already applied to the same task generation.
    Replayed,
    /// The resolution was durably audited but its generation is stale or terminal.
    AuditedStale,
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

/// Idempotent acknowledgement returned to a compensation-review dispatcher.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCompensationReviewAcknowledgement {
    /// The resolution was applied to the current compensation generation.
    Applied,
    /// This review UID was already applied to the same compensation generation.
    Replayed,
    /// The resolution was durably audited but its generation is stale or settled.
    AuditedStale,
}

/// Internal request that starts the keyed run workflow.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunWorkflowRequest {
    /// Durable run identifier and workflow key.
    pub run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Parent session.
    pub session_id: SessionId,
    /// Exact authenticated identity admitted when the run was launched.
    pub identity: Identity,
}

/// Internal request that notifies a keyed run of a persisted scheduling change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunWakeRequest {
    /// Durable run identifier and workflow key.
    pub run_uid: Uuid,
    /// Exact persisted monotonic wake epoch.
    pub wake_epoch: u64,
    /// Mutation that caused the wake.
    pub reason: ExecutionRunWakeReason,
}

/// Stable reasons a run workflow is awakened.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunWakeReason {
    /// A task persisted an outcome.
    TaskOutcome,
    /// The user confirmed the active plan and budget.
    Confirmed,
    /// Audience-bound input resumed a task.
    InputDelivered,
    /// An explicit review task was resolved.
    ReviewDecided,
    /// A named signal was delivered.
    SignalDelivered,
    /// An externally supplied amendment was accepted.
    AmendmentAccepted,
    /// The run was cancelled.
    Cancelled,
    /// A compensation registration or generation changed durably.
    CompensationProgress,
}

/// Internal request that dispatches one keyed task generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskWorkflowRequest {
    /// Owning run.
    pub run_uid: Uuid,
    /// Stable workflow key.
    pub task_id: ExecutionTaskId,
    /// Current generation fence.
    pub generation: u64,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Parent session used for policy and model context.
    pub session_id: SessionId,
    /// Exact authenticated identity inherited from the owning run workflow.
    pub identity: Identity,
}

/// Internal request that dispatches one keyed compensation generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCompensationWorkflowRequest {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable workflow key derived from the forward task.
    pub compensation_id: CompensationId,
    /// Current compensation generation fence.
    pub generation: u64,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Parent session used for policy and action context.
    pub session_id: SessionId,
    /// Exact authenticated identity inherited from the owning run workflow.
    pub identity: Identity,
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
