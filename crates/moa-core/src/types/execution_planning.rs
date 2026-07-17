//! Cycle-free execution routing, planning-audit, provenance, and admission DTOs.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_canonical_json::CanonicalFormatter;
use serde_json::Value;
use uuid::Uuid;

use crate::types::{
    contact::ContactId,
    identifiers::{SessionId, TenantId},
};

/// Maximum UTF-8 bytes accepted for one planner candidate document.
pub const EXECUTION_CANDIDATE_MAX_BYTES: usize = 1_048_576;
/// Maximum UTF-8 bytes accepted for one bounded audit report document.
pub const EXECUTION_REPORT_MAX_BYTES: usize = 262_144;
/// Maximum encoded bytes accepted for one complete planning-audit envelope.
pub const EXECUTION_AUDIT_ENVELOPE_MAX_BYTES: usize = 4_194_304;
/// Maximum number of retained violations in a bounded audit report.
pub const EXECUTION_AUDIT_MAX_VIOLATIONS: usize = 256;

/// Stable execution mode selected for one root user turn.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Produce one model response without tools or planning.
    Respond,
    /// Run the bounded interactive model/tool loop.
    Act,
    /// Compile or instantiate and detach a durable execution run.
    Run,
}

/// Stable primary reason for one execution-routing decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRouteReason {
    /// The request needs only a direct response.
    SimpleResponse,
    /// The request is bounded interactive work.
    BoundedInteractiveWork,
    /// Required preflight information is missing.
    PreflightInputMissing,
    /// The user explicitly requested durable run execution.
    ExplicitRun,
    /// The request spans a bulk collection.
    BulkCollection,
    /// The work must survive or resume across process lifetimes.
    DurableOrResumable,
    /// The work has execution-run-shaped fan-out.
    HighFanout,
    /// The work requires an approval or external signal wait.
    ApprovalOrSignal,
    /// The caller selected one exact pinned execution template.
    SelectedExecutionTemplate,
    /// A bounded Act turn discovered run-shaped work.
    ActEscalation,
}

/// Final execution-routing outcome, including preflight clarification.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionRouteDecision {
    /// Routing cannot proceed until missing input is supplied.
    NeedsInput {
        /// Stable reason for the input request.
        reason: ExecutionRouteReason,
        /// Bounded concrete inputs the caller must supply.
        missing_inputs: Vec<String>,
    },
    /// The turn was assigned one execution mode.
    Routed {
        /// Selected execution mode.
        mode: ExecutionMode,
        /// Stable primary routing reason.
        reason: ExecutionRouteReason,
    },
}

/// Trusted or model-assisted source of one execution route.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRouteSource {
    /// One strict auxiliary-model classification.
    Classifier,
    /// Deterministic preflight for an empty objective.
    BlankObjective,
    /// Exact trusted execution-template invocation.
    SelectedExecutionTemplate,
    /// Validated typed escalation from an Act turn.
    ActEscalation,
}

/// Closed outcome of the optional execution-route classifier call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRouteClassifierOutcome {
    /// No classifier call was needed for a trusted route.
    NotCalled,
    /// A strict classifier response was accepted.
    Accepted,
    /// The provider rejected the request before a stream was available.
    ProviderError,
    /// The provider stream failed before a complete response was available.
    StreamError,
    /// The collected classifier response exceeded its byte cap.
    Oversized,
    /// The collected response did not match the strict response schema.
    SchemaRejected,
    /// The response label, reason, or missing-input fields were inconsistent.
    InvalidDecision,
    /// A risky classifier decision did not meet its confidence threshold.
    LowConfidence,
    /// Existing attachments or a recent target required bounded Act context.
    ContextForcedAct,
}

/// Normalized token usage retained for one execution-route classifier call.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRouteUsageV1 {
    /// Uncached provider input tokens.
    pub input_tokens_uncached: u64,
    /// Provider input tokens used to populate a prompt cache.
    pub input_tokens_cache_write: u64,
    /// Provider input tokens served from a prompt cache.
    pub input_tokens_cache_read: u64,
    /// Provider output tokens.
    pub output_tokens: u64,
}

impl ExecutionRouteUsageV1 {
    /// Returns whether every normalized usage counter is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.input_tokens_uncached == 0
            && self.input_tokens_cache_write == 0
            && self.input_tokens_cache_read == 0
            && self.output_tokens == 0
    }
}

/// Redacted provenance for one final execution-routing decision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRouteProvenanceV1 {
    /// Trusted bypass or classifier source.
    pub source: ExecutionRouteSource,
    /// Closed outcome of the optional classifier call.
    pub classifier_outcome: ExecutionRouteClassifierOutcome,
    /// Requested or actual provider model for an attempted classifier call.
    pub provider_model: Option<String>,
    /// Stable classifier prompt version, when a classifier call was attempted.
    pub prompt_version: Option<String>,
    /// Domain-separated hash of the exact objective.
    pub objective_hash: String,
    /// Domain-separated hash of collected classifier text, when available.
    pub response_hash: Option<String>,
    /// Model-reported confidence in basis points, when strict parsing succeeded.
    pub confidence_bps: Option<u16>,
    /// Number of bounded missing-input entries returned to the caller.
    pub missing_input_count: u8,
    /// Normalized provider token usage.
    pub usage: ExecutionRouteUsageV1,
    /// Classifier cost in micro-US-dollars, computed at the provider-owning boundary.
    pub cost_microusd: u64,
    /// Measured classifier call duration.
    pub duration_micros: u64,
}

/// Final execution route plus its redacted provenance.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRoutingResultV1 {
    /// Selected route or bounded clarification.
    pub decision: ExecutionRouteDecision,
    /// Redacted route provenance and optional classifier measurements.
    pub provenance: ExecutionRouteProvenanceV1,
}

/// Bounded evidence gathered by an Act turn before escalation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanningEvidence {
    /// Stable evidence source label.
    pub source: String,
    /// Concise evidence summary.
    pub summary: String,
    /// Structured bounded evidence value.
    pub value: Value,
}

/// Typed request to escalate a bounded Act turn into a durable run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActEscalationSignal {
    /// Exact user-derived run objective.
    pub objective: String,
    /// Stable run-shaped reason discovered by Act.
    pub reason: ExecutionRouteReason,
    /// Bounded structured evidence already gathered by the turn.
    pub evidence: Vec<ExecutionPlanningEvidence>,
}

impl ActEscalationSignal {
    /// Validates all deterministic evidence bounds without truncation.
    pub fn validate(&self) -> Result<(), ExecutionPlanningContractError> {
        ensure_bytes("objective", &self.objective, 4_096)?;
        if self.evidence.len() > 32 {
            return Err(ExecutionPlanningContractError::BoundExceeded {
                field: "evidence".to_string(),
                limit: 32,
                observed: self.evidence.len(),
            });
        }
        for (index, evidence) in self.evidence.iter().enumerate() {
            ensure_bytes(&format!("evidence[{index}].source"), &evidence.source, 256)?;
            ensure_bytes(
                &format!("evidence[{index}].summary"),
                &evidence.summary,
                4_096,
            )?;
            let bytes = canonical_json_bytes(&evidence.value)?;
            if bytes.len() > 16_384 {
                return Err(ExecutionPlanningContractError::BoundExceeded {
                    field: format!("evidence[{index}].value"),
                    limit: 16_384,
                    observed: bytes.len(),
                });
            }
        }
        let vector_bytes = canonical_json_bytes(&self.evidence)?;
        if vector_bytes.len() > 262_144 {
            return Err(ExecutionPlanningContractError::BoundExceeded {
                field: "evidence".to_string(),
                limit: 262_144,
                observed: vector_bytes.len(),
            });
        }
        Ok(())
    }
}

/// Exact pinned execution-template revision selected by a caller.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedExecutionTemplateRef {
    /// Canonical artifact-reference string.
    pub skill_ref: String,
    /// Exact pinned skill revision.
    pub revision_uid: Uuid,
}

/// Structured invocation of one exact pinned execution template.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTemplateInvocation {
    /// Exact pinned template revision.
    pub template: PinnedExecutionTemplateRef,
    /// Structured invocation input validated against both schemas.
    pub input: Value,
}

/// Planning route stage recorded in durable audit history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRouteStage {
    /// Initial root-turn route.
    Initial,
    /// Typed Act-to-Run escalation route.
    ActEscalation,
}

/// Closed route-decision category stored in audit history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRouteDecisionKind {
    /// Missing input prevented routing.
    NeedsInput,
    /// A concrete execution mode was selected.
    Routed,
}

/// Closed planner-call category stored in audit history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPlannerCallKind {
    /// Initial plan generation.
    InitialPlan,
    /// Sole permitted repair for an initial compiler rejection.
    InitialRepair,
    /// Plan-amendment generation.
    Amendment,
    /// Sole permitted repair for an amendment compiler rejection.
    AmendmentRepair,
}

/// Closed planner-call outcome stored in audit history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPlannerOutcome {
    /// The candidate was accepted.
    Accepted,
    /// Planning identified missing caller input.
    NeedsInput,
    /// Planning determined the requested work is unsupported.
    Unsupported,
    /// Provider output failed the strict response schema.
    SchemaRejected,
    /// Initial repair changed the immutable goal contract.
    ImmutableGoalChanged,
    /// The pure compiler rejected the strict candidate.
    CompilerRejected,
    /// The collected provider response exceeded the candidate byte cap.
    Oversized,
    /// The provider call failed before a candidate was available.
    ProviderError,
}

/// Source cohort for one pure execution compiler call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCompileSource {
    /// One-off model-generated plan.
    GeneratedPlan,
    /// Exact pinned skill execution template.
    SkillTemplate,
    /// Experiment-owned pinned template.
    ExperimentTemplate,
    /// Structured plan amendment.
    Amendment,
    /// Skill-regression validation compile.
    SkillRegression,
}

/// Closed pure compiler outcome stored in audit history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCompileOutcome {
    /// Compilation produced an accepted canonical plan.
    Accepted,
    /// Compilation identified missing structured input.
    NeedsInput,
    /// Compilation found no supported serving path.
    Unsupported,
    /// Compilation rejected the candidate.
    Rejected,
}

/// Recursively strict v1 execution-planning audit event.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanningAuditEnvelopeV1 {
    /// Audit schema version, fixed at `1`.
    pub schema_version: u8,
    /// Tenant that owns this audit record.
    #[schemars(with = "Uuid")]
    pub tenant_id: TenantId,
    /// Optional contact scope for this audit record.
    #[schemars(with = "Option<Uuid>")]
    pub contact_id: Option<ContactId>,
    /// Session destination for session-bound records.
    #[schemars(with = "Option<Uuid>")]
    pub session_id: Option<SessionId>,
    /// Originating persisted user-event sequence for session-bound records.
    pub originating_sequence: Option<u64>,
    /// Closed route, planner-call, or compiler payload.
    pub payload: ExecutionPlanningAuditPayloadV1,
}

/// Closed v1 execution-planning audit payload.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionPlanningAuditPayloadV1 {
    /// One trusted or model-assisted routing decision.
    Route {
        /// Initial or Act-escalation route stage.
        stage: ExecutionRouteStage,
        /// Needs-input or routed outcome.
        decision: ExecutionRouteDecisionKind,
        /// Selected execution mode for routed decisions.
        mode: Option<ExecutionMode>,
        /// Stable primary route reason.
        reason: ExecutionRouteReason,
        /// Redacted trusted-bypass or classifier provenance.
        provenance: ExecutionRouteProvenanceV1,
        /// Durable acceptance timestamp.
        accepted_at: DateTime<Utc>,
    },
    /// One actual provider planner call.
    PlannerCall {
        /// Initial, repair, amendment, or amendment-repair call.
        call_kind: ExecutionPlannerCallKind,
        /// Zero for the first call and one for its sole repair.
        call_ordinal: u8,
        /// Run identifier for amendment calls.
        run_uid: Option<Uuid>,
        /// Plan revision for amendment calls.
        plan_revision: Option<u64>,
        /// Closed planner outcome.
        outcome: ExecutionPlannerOutcome,
        /// Provider model identifier.
        provider_model: String,
        /// Stable planner prompt version.
        prompt_version: String,
        /// Candidate or raw-response hash when required by the outcome.
        candidate_hash: Option<String>,
        /// Canonical strict candidate JSON when required by the outcome.
        candidate_json: Option<String>,
        /// Canonical bounded audit report when required by the outcome.
        compiler_report: Option<String>,
        /// Measured provider-call duration.
        duration_micros: u64,
        /// Durable call timestamp.
        created_at: DateTime<Utc>,
    },
    /// One actual pure compiler call.
    Compile {
        /// Compiler input cohort.
        source: ExecutionCompileSource,
        /// Stable source-specific operation key.
        operation_key: String,
        /// Optional run identifier for amendments.
        run_uid: Option<Uuid>,
        /// Optional base plan revision for amendments.
        plan_revision: Option<u64>,
        /// Closed compiler outcome.
        outcome: ExecutionCompileOutcome,
        /// Hash of the closed canonical compile candidate.
        candidate_hash: String,
        /// Canonical accepted plan hash, when compilation succeeded.
        final_plan_hash: Option<String>,
        /// Canonical bounded compiler report document.
        validation_report: String,
        /// Measured compiler duration.
        duration_micros: u64,
        /// Durable compile timestamp.
        created_at: DateTime<Utc>,
    },
}

/// Bounded audit report attached to planner and compiler records.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionAuditReportV1 {
    /// Strict-schema violations.
    Schema {
        /// Sorted retained violations.
        violations: Vec<ExecutionAuditViolationV1>,
        /// Number of sorted violations omitted after the fixed cap.
        omitted_violations: u32,
        /// Hash of the complete sorted unbounded report.
        full_report_hash: String,
    },
    /// Pure compiler violations.
    Compiler {
        /// Sorted retained violations.
        violations: Vec<ExecutionAuditViolationV1>,
        /// Number of sorted violations omitted after the fixed cap.
        omitted_violations: u32,
        /// Hash of the complete sorted unbounded report.
        full_report_hash: String,
    },
    /// Provider response exceeded a strict byte cap.
    Oversized {
        /// Oversized planner field.
        field: ExecutionOversizedAuditField,
        /// Configured UTF-8 byte cap.
        limit_bytes: u64,
        /// Observed UTF-8 bytes.
        observed_bytes: u64,
        /// Domain-separated hash of the oversized content.
        content_hash: String,
    },
}

/// One bounded schema or compiler violation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAuditViolationV1 {
    /// Stable violation code.
    pub code: String,
    /// JSON Pointer-like violation path.
    pub path: String,
    /// Concise violation message.
    pub message: String,
}

/// Fields that may produce an oversized planner audit outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOversizedAuditField {
    /// Collected provider candidate response.
    Candidate,
}

/// Admission status persisted in an execution-run-started event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunAdmissionStatus {
    /// Run is queued for detached execution.
    Queued,
    /// Run is persisted but awaits owning-user confirmation.
    AwaitingConfirmation,
}

/// Fixed methodology for the displayed execution admission estimate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEstimateMethodologyV1 {
    /// Retry- and fan-out-inclusive compiler worst case.
    ConservativeWorstCaseV1,
}

/// Exact compiler estimate displayed before execution confirmation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAdmissionEstimateV1 {
    /// Worst-case cost in micro-US-dollars.
    pub cost_microusd: u64,
    /// Worst-case model tokens.
    pub tokens: u64,
    /// Worst-case logical tasks.
    pub tasks: u64,
    /// Worst-case governed tool calls.
    pub tool_calls: u64,
    /// Worst-case retrieved bytes.
    pub retrieved_bytes: u64,
}

/// Evidence required to confirm an above-threshold admitted run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfirmationEvidenceV1 {
    /// Exact active plan hash shown to the owning user.
    pub active_plan_hash: String,
    /// Exact compiler estimate shown to the owning user.
    pub estimate: ExecutionAdmissionEstimateV1,
    /// Fixed estimate methodology.
    pub methodology: ExecutionEstimateMethodologyV1,
}

/// Minimal durable payload published after a committed run admission.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunStarted {
    /// Committed execution-run identifier.
    pub run_uid: Uuid,
    /// Exact persisted user event that originated the run.
    pub originating_user_sequence_num: u64,
    /// Active plan revision at admission.
    pub plan_revision: u64,
    /// Queued or awaiting-confirmation admission status.
    pub status: ExecutionRunAdmissionStatus,
    /// Required only for awaiting-confirmation admissions.
    pub confirmation: Option<ExecutionConfirmationEvidenceV1>,
}

impl ExecutionRunStarted {
    /// Validates the closed status/evidence matrix and plan-hash representation.
    pub fn validate(&self) -> Result<(), ExecutionPlanningContractError> {
        if self.originating_user_sequence_num == 0 {
            return Err(ExecutionPlanningContractError::InvalidField {
                field: "originating_user_sequence_num".to_string(),
                message: "must be greater than zero".to_string(),
            });
        }
        if self.plan_revision == 0 {
            return Err(ExecutionPlanningContractError::InvalidField {
                field: "plan_revision".to_string(),
                message: "must be greater than zero".to_string(),
            });
        }
        match (&self.status, &self.confirmation) {
            (ExecutionRunAdmissionStatus::Queued, None) => Ok(()),
            (ExecutionRunAdmissionStatus::AwaitingConfirmation, Some(evidence)) => {
                validate_hash("confirmation.active_plan_hash", &evidence.active_plan_hash)
            }
            (ExecutionRunAdmissionStatus::Queued, Some(_)) => {
                Err(ExecutionPlanningContractError::InvalidField {
                    field: "confirmation".to_string(),
                    message: "queued admission must not include confirmation evidence".to_string(),
                })
            }
            (ExecutionRunAdmissionStatus::AwaitingConfirmation, None) => {
                Err(ExecutionPlanningContractError::InvalidField {
                    field: "confirmation".to_string(),
                    message: "awaiting_confirmation admission requires confirmation evidence"
                        .to_string(),
                })
            }
        }
    }
}

/// Exact generated-plan planner provenance.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedPlanPlannerProvenanceV1 {
    /// Non-empty provider model identifier.
    pub model: String,
    /// Non-empty planner prompt version.
    pub prompt_version: String,
    /// Accepted strict candidate hash.
    pub candidate_hash: String,
    /// Accepted compiler full-report hash.
    pub compiler_report_hash: String,
    /// Committed active plan hash.
    pub final_plan_hash: String,
    /// Zero or one compiler-repair calls.
    pub repair_attempts: u8,
}

/// Closed source provenance persisted for every admitted execution run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionSourceProvenanceV1 {
    /// One-off strict generated plan.
    GeneratedPlan {
        /// Stable run-shaped routing reason.
        route_reason: ExecutionRouteReason,
        /// Exact accepted planner/compiler provenance.
        planner: GeneratedPlanPlannerProvenanceV1,
    },
    /// Exact pinned skill execution template.
    SkillTemplate {
        /// Fixed selected-template route reason.
        route_reason: ExecutionRouteReason,
        /// Byte-identical canonical skill artifact reference.
        skill_template_ref: String,
        /// Exact pinned skill revision UUID.
        skill_template_revision_uid: Uuid,
    },
    /// Exact pinned execution template invoked by a behavior-lab experiment.
    ExperimentTemplate {
        /// Fixed explicit-run route reason.
        route_reason: ExecutionRouteReason,
        /// Byte-identical canonical skill artifact reference.
        skill_template_ref: String,
        /// Exact pinned skill revision UUID.
        skill_template_revision_uid: Uuid,
        /// Owning experiment-run UUID.
        experiment_run_uid: Uuid,
        /// Owning score-run UUID.
        score_run_id: Uuid,
        /// Exact trial UUID, or explicit null for a run target.
        #[serde(deserialize_with = "deserialize_required_nullable_uuid")]
        trial_uid: Option<Uuid>,
    },
}

impl ExecutionSourceProvenanceV1 {
    /// Validates the exact generated-plan, skill-template, and experiment-template cohorts.
    pub fn validate(
        &self,
        committed_plan_hash: &str,
    ) -> Result<(), ExecutionPlanningContractError> {
        validate_hash("committed_plan_hash", committed_plan_hash)?;
        match self {
            Self::GeneratedPlan {
                route_reason,
                planner,
            } => {
                if !matches!(
                    route_reason,
                    ExecutionRouteReason::ExplicitRun
                        | ExecutionRouteReason::BulkCollection
                        | ExecutionRouteReason::DurableOrResumable
                        | ExecutionRouteReason::HighFanout
                        | ExecutionRouteReason::ApprovalOrSignal
                        | ExecutionRouteReason::ActEscalation
                ) {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "route_reason".to_string(),
                        message: "generated_plan requires a run-shaped route reason".to_string(),
                    });
                }
                ensure_nonempty_bytes("planner.model", &planner.model, 128)?;
                ensure_nonempty_bytes("planner.prompt_version", &planner.prompt_version, 64)?;
                validate_hash("planner.candidate_hash", &planner.candidate_hash)?;
                validate_hash(
                    "planner.compiler_report_hash",
                    &planner.compiler_report_hash,
                )?;
                validate_hash("planner.final_plan_hash", &planner.final_plan_hash)?;
                if planner.final_plan_hash != committed_plan_hash {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "planner.final_plan_hash".to_string(),
                        message: "final plan hash must equal the committed active plan hash"
                            .to_string(),
                    });
                }
                if planner.repair_attempts > 1 {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "planner.repair_attempts".to_string(),
                        message: "repair_attempts must be exactly 0 or 1".to_string(),
                    });
                }
            }
            Self::SkillTemplate {
                route_reason,
                skill_template_ref,
                ..
            } => {
                if *route_reason != ExecutionRouteReason::SelectedExecutionTemplate {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "route_reason".to_string(),
                        message: "skill_template requires selected_execution_template".to_string(),
                    });
                }
                ensure_nonempty_bytes("skill_template_ref", skill_template_ref, 535)?;
            }
            Self::ExperimentTemplate {
                route_reason,
                skill_template_ref,
                ..
            } => {
                if *route_reason != ExecutionRouteReason::ExplicitRun {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "route_reason".to_string(),
                        message: "experiment_template requires explicit_run".to_string(),
                    });
                }
                ensure_nonempty_bytes("skill_template_ref", skill_template_ref, 535)?;
            }
        }
        Ok(())
    }
}

fn deserialize_required_nullable_uuid<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer)
}

/// Stable semantic result returned by Session planning-audit append.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionPlanningAuditWriteResult {
    /// The record was inserted.
    Applied {
        /// First persisted envelope.
        envelope: ExecutionPlanningAuditEnvelopeV1,
    },
    /// The exact semantic record already existed.
    Replayed {
        /// First persisted envelope with its original measurements.
        envelope: ExecutionPlanningAuditEnvelopeV1,
    },
    /// The logical key already exists with a different semantic payload.
    Conflict,
}

/// Typed execution-planning contract error.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExecutionPlanningContractError {
    /// A bounded field exceeded its fixed limit.
    #[error("execution planning field {field} exceeds limit {limit}: {observed}")]
    BoundExceeded {
        /// Field path.
        field: String,
        /// Maximum bytes or entries.
        limit: usize,
        /// Observed bytes or entries.
        observed: usize,
    },
    /// A field violates its closed representation contract.
    #[error("invalid execution planning field {field}: {message}")]
    InvalidField {
        /// Field path.
        field: String,
        /// Violation detail.
        message: String,
    },
    /// Canonical JSON serialization failed.
    #[error("execution planning canonical JSON: {0}")]
    Json(String),
}

/// Computes a lowercase domain-separated BLAKE3 digest.
#[must_use]
pub fn execution_planning_hash(domain: &str, bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

/// Builds the exact planning-audit dedupe key from its producer-owned logical tuple.
pub fn execution_planning_dedupe_key(
    envelope: &ExecutionPlanningAuditEnvelopeV1,
) -> Result<String, ExecutionPlanningContractError> {
    let tenant = envelope.tenant_id.to_string();
    let contact = envelope.contact_id.map(|id| id.to_string());
    let session = envelope.session_id.map(|id| id.to_string());
    let origin = envelope.originating_sequence.map(|value| value.to_string());
    let fields = match &envelope.payload {
        ExecutionPlanningAuditPayloadV1::Route { stage, .. } => vec![
            Some("route".to_string()),
            Some(tenant),
            contact,
            session,
            origin,
            Some(canonical_enum_string(stage)?),
        ],
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind,
            call_ordinal,
            run_uid,
            plan_revision,
            ..
        } => vec![
            Some("planner".to_string()),
            Some(tenant),
            contact,
            session,
            origin,
            run_uid.map(|uid| uid.to_string()),
            plan_revision.map(|value| value.to_string()),
            Some(canonical_enum_string(call_kind)?),
            Some(call_ordinal.to_string()),
        ],
        ExecutionPlanningAuditPayloadV1::Compile {
            source,
            operation_key,
            ..
        } => vec![
            Some("compile".to_string()),
            Some(tenant),
            contact,
            Some(canonical_enum_string(source)?),
            Some(operation_key.clone()),
        ],
    };
    dedupe_key_from_fields(&fields)
}

fn dedupe_key_from_fields(
    fields: &[Option<String>],
) -> Result<String, ExecutionPlanningContractError> {
    let mut bytes = Vec::new();
    for field in fields {
        match field {
            Some(value) => {
                bytes.push(1);
                let length = u32::try_from(value.len()).map_err(|_| {
                    ExecutionPlanningContractError::BoundExceeded {
                        field: "dedupe_tuple_field".to_string(),
                        limit: u32::MAX as usize,
                        observed: value.len(),
                    }
                })?;
                bytes.extend_from_slice(&length.to_be_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            None => bytes.push(0),
        }
    }
    Ok(format!(
        "execution-planning-v1:{}",
        execution_planning_hash("moa.execution.planning-audit-dedupe.v1", &bytes)
    ))
}

/// Returns whether two audit envelopes are semantically equal for idempotent replay.
#[must_use]
pub fn planning_audit_semantically_equal(
    left: &ExecutionPlanningAuditEnvelopeV1,
    right: &ExecutionPlanningAuditEnvelopeV1,
) -> bool {
    if left.schema_version != right.schema_version
        || left.tenant_id != right.tenant_id
        || left.contact_id != right.contact_id
        || left.session_id != right.session_id
        || left.originating_sequence != right.originating_sequence
    {
        return false;
    }
    match (&left.payload, &right.payload) {
        (
            ExecutionPlanningAuditPayloadV1::Route {
                stage: left_stage,
                decision: left_decision,
                mode: left_mode,
                reason: left_reason,
                provenance: left_provenance,
                ..
            },
            ExecutionPlanningAuditPayloadV1::Route {
                stage: right_stage,
                decision: right_decision,
                mode: right_mode,
                reason: right_reason,
                provenance: right_provenance,
                ..
            },
        ) => {
            (left_stage, left_decision, left_mode, left_reason)
                == (right_stage, right_decision, right_mode, right_reason)
                && route_provenance_semantically_equal(left_provenance, right_provenance)
        }
        (
            ExecutionPlanningAuditPayloadV1::PlannerCall {
                call_kind: left_kind,
                call_ordinal: left_ordinal,
                run_uid: left_run,
                plan_revision: left_revision,
                outcome: left_outcome,
                provider_model: left_model,
                prompt_version: left_prompt,
                candidate_hash: left_hash,
                candidate_json: left_candidate,
                compiler_report: left_report,
                ..
            },
            ExecutionPlanningAuditPayloadV1::PlannerCall {
                call_kind: right_kind,
                call_ordinal: right_ordinal,
                run_uid: right_run,
                plan_revision: right_revision,
                outcome: right_outcome,
                provider_model: right_model,
                prompt_version: right_prompt,
                candidate_hash: right_hash,
                candidate_json: right_candidate,
                compiler_report: right_report,
                ..
            },
        ) => {
            (
                left_kind,
                left_ordinal,
                left_run,
                left_revision,
                left_outcome,
                left_model,
                left_prompt,
                left_hash,
                left_candidate,
                left_report,
            ) == (
                right_kind,
                right_ordinal,
                right_run,
                right_revision,
                right_outcome,
                right_model,
                right_prompt,
                right_hash,
                right_candidate,
                right_report,
            )
        }
        (
            ExecutionPlanningAuditPayloadV1::Compile {
                source: left_source,
                operation_key: left_key,
                run_uid: left_run,
                plan_revision: left_revision,
                outcome: left_outcome,
                candidate_hash: left_candidate,
                final_plan_hash: left_plan,
                validation_report: left_report,
                ..
            },
            ExecutionPlanningAuditPayloadV1::Compile {
                source: right_source,
                operation_key: right_key,
                run_uid: right_run,
                plan_revision: right_revision,
                outcome: right_outcome,
                candidate_hash: right_candidate,
                final_plan_hash: right_plan,
                validation_report: right_report,
                ..
            },
        ) => {
            (
                left_source,
                left_key,
                left_run,
                left_revision,
                left_outcome,
                left_candidate,
                left_plan,
                left_report,
            ) == (
                right_source,
                right_key,
                right_run,
                right_revision,
                right_outcome,
                right_candidate,
                right_plan,
                right_report,
            )
        }
        _ => false,
    }
}

/// Validates one strict planning-audit envelope and all outcome-specific fields.
pub fn validate_planning_audit_envelope(
    envelope: &ExecutionPlanningAuditEnvelopeV1,
) -> Result<(), ExecutionPlanningContractError> {
    if envelope.schema_version != 1 {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "schema_version".to_string(),
            message: "must equal 1".to_string(),
        });
    }
    let session_bound = envelope.session_id.is_some() && envelope.originating_sequence.is_some();
    if envelope.session_id.is_some() != envelope.originating_sequence.is_some() {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "session_id".to_string(),
            message: "session_id and originating_sequence must be present together".to_string(),
        });
    }
    match &envelope.payload {
        ExecutionPlanningAuditPayloadV1::Route {
            stage,
            decision,
            mode,
            reason,
            provenance,
            ..
        } => {
            if !session_bound {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "session_id".to_string(),
                    message: "route records must be session-bound".to_string(),
                });
            }
            validate_route_audit(*stage, *decision, *mode, *reason, provenance)?;
        }
        ExecutionPlanningAuditPayloadV1::PlannerCall {
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
        } => {
            if !session_bound {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "session_id".to_string(),
                    message: "planner-call records must be session-bound".to_string(),
                });
            }
            let repair = matches!(
                call_kind,
                ExecutionPlannerCallKind::InitialRepair | ExecutionPlannerCallKind::AmendmentRepair
            );
            if *call_ordinal != u8::from(repair) {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.call_ordinal".to_string(),
                    message: "initial calls require ordinal 0 and repairs require ordinal 1"
                        .to_string(),
                });
            }
            let amendment = matches!(
                call_kind,
                ExecutionPlannerCallKind::Amendment | ExecutionPlannerCallKind::AmendmentRepair
            );
            if amendment != (run_uid.is_some() && plan_revision.is_some()) {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.run_uid".to_string(),
                    message: "run UID and plan revision are required only for amendment calls"
                        .to_string(),
                });
            }
            ensure_nonempty_bytes("payload.provider_model", provider_model, 128)?;
            ensure_nonempty_bytes("payload.prompt_version", prompt_version, 64)?;
            let requires_candidate = !matches!(outcome, ExecutionPlannerOutcome::ProviderError);
            if requires_candidate != candidate_hash.is_some() {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.candidate_hash".to_string(),
                    message: "candidate hash nullability does not match planner outcome"
                        .to_string(),
                });
            }
            if let Some(hash) = candidate_hash {
                validate_hash("payload.candidate_hash", hash)?;
            }
            let canonical_candidate = matches!(
                outcome,
                ExecutionPlannerOutcome::Accepted
                    | ExecutionPlannerOutcome::NeedsInput
                    | ExecutionPlannerOutcome::Unsupported
                    | ExecutionPlannerOutcome::ImmutableGoalChanged
                    | ExecutionPlannerOutcome::CompilerRejected
            );
            if canonical_candidate != candidate_json.is_some() {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.candidate_json".to_string(),
                    message: "candidate JSON nullability does not match planner outcome"
                        .to_string(),
                });
            }
            if let Some(candidate) = candidate_json {
                ensure_bytes(
                    "payload.candidate_json",
                    candidate,
                    EXECUTION_CANDIDATE_MAX_BYTES,
                )?;
                validate_canonical_document("payload.candidate_json", candidate)?;
                let expected = execution_planning_hash(
                    "moa.execution.planner-candidate.v1",
                    candidate.as_bytes(),
                );
                if candidate_hash.as_deref() != Some(expected.as_str()) {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "payload.candidate_hash".to_string(),
                        message: "must hash the canonical strict candidate".to_string(),
                    });
                }
            }
            let requires_report = !matches!(outcome, ExecutionPlannerOutcome::ProviderError);
            if requires_report != compiler_report.is_some() {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.compiler_report".to_string(),
                    message: "compiler report nullability does not match planner outcome"
                        .to_string(),
                });
            }
            if let Some(report) = compiler_report {
                ensure_bytes(
                    "payload.compiler_report",
                    report,
                    EXECUTION_REPORT_MAX_BYTES,
                )?;
                let parsed = validate_canonical_report("payload.compiler_report", report)?;
                let valid_kind = matches!(
                    (outcome, parsed),
                    (
                        ExecutionPlannerOutcome::SchemaRejected
                            | ExecutionPlannerOutcome::ImmutableGoalChanged,
                        ExecutionAuditReportV1::Schema { .. }
                    ) | (
                        ExecutionPlannerOutcome::Accepted
                            | ExecutionPlannerOutcome::NeedsInput
                            | ExecutionPlannerOutcome::Unsupported
                            | ExecutionPlannerOutcome::CompilerRejected,
                        ExecutionAuditReportV1::Compiler { .. }
                    ) | (
                        ExecutionPlannerOutcome::Oversized,
                        ExecutionAuditReportV1::Oversized { .. }
                    )
                );
                if !valid_kind {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "payload.compiler_report".to_string(),
                        message: "report kind does not match planner outcome".to_string(),
                    });
                }
            }
            if *outcome == ExecutionPlannerOutcome::ImmutableGoalChanged
                && *call_kind != ExecutionPlannerCallKind::InitialRepair
            {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.outcome".to_string(),
                    message: "immutable_goal_changed is legal only for initial_repair".to_string(),
                });
            }
        }
        ExecutionPlanningAuditPayloadV1::Compile {
            source,
            run_uid,
            plan_revision,
            candidate_hash,
            final_plan_hash,
            validation_report,
            outcome,
            operation_key,
            ..
        } => {
            let requires_session = !matches!(source, ExecutionCompileSource::SkillRegression);
            if requires_session != session_bound {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "session_id".to_string(),
                    message: "compile destination does not match its source".to_string(),
                });
            }
            let amendment = *source == ExecutionCompileSource::Amendment;
            if amendment != (run_uid.is_some() && plan_revision.is_some()) {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.run_uid".to_string(),
                    message: "run UID and plan revision are required only for amendments"
                        .to_string(),
                });
            }
            validate_hash("payload.candidate_hash", candidate_hash)?;
            if (*outcome == ExecutionCompileOutcome::Accepted) != final_plan_hash.is_some() {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.final_plan_hash".to_string(),
                    message: "final plan hash is required only for accepted compiles".to_string(),
                });
            }
            if let Some(hash) = final_plan_hash {
                validate_hash("payload.final_plan_hash", hash)?;
            }
            ensure_nonempty_bytes("payload.operation_key", operation_key, 512)?;
            if let (ExecutionCompileSource::Amendment, Some(run_uid), Some(plan_revision)) =
                (source, run_uid, plan_revision)
            {
                let expected = format!("run:{run_uid}:{plan_revision}:amendment:{candidate_hash}");
                if operation_key != &expected {
                    return Err(ExecutionPlanningContractError::InvalidField {
                        field: "payload.operation_key".to_string(),
                        message: "amendment operation key must bind the run, revision, and compile candidate hash"
                            .to_string(),
                    });
                }
            }
            ensure_bytes(
                "payload.validation_report",
                validation_report,
                EXECUTION_REPORT_MAX_BYTES,
            )?;
            if !matches!(
                validate_canonical_report("payload.validation_report", validation_report)?,
                ExecutionAuditReportV1::Compiler { .. }
            ) {
                return Err(ExecutionPlanningContractError::InvalidField {
                    field: "payload.validation_report".to_string(),
                    message: "compile records require a compiler report".to_string(),
                });
            }
        }
    }
    let encoded = serde_json::to_vec(envelope)
        .map_err(|error| ExecutionPlanningContractError::Json(error.to_string()))?;
    if encoded.len() > EXECUTION_AUDIT_ENVELOPE_MAX_BYTES {
        return Err(ExecutionPlanningContractError::BoundExceeded {
            field: "envelope".to_string(),
            limit: EXECUTION_AUDIT_ENVELOPE_MAX_BYTES,
            observed: encoded.len(),
        });
    }
    Ok(())
}

/// Returns whether two route-provenance records carry the same replay semantics.
///
/// Measured duration is deliberately excluded because replay can reproduce the
/// same provider result through a different local timing path.
#[must_use]
pub fn route_provenance_semantically_equal(
    left: &ExecutionRouteProvenanceV1,
    right: &ExecutionRouteProvenanceV1,
) -> bool {
    left.source == right.source
        && left.classifier_outcome == right.classifier_outcome
        && left.provider_model == right.provider_model
        && left.prompt_version == right.prompt_version
        && left.objective_hash == right.objective_hash
        && left.response_hash == right.response_hash
        && left.confidence_bps == right.confidence_bps
        && left.missing_input_count == right.missing_input_count
        && left.usage == right.usage
        && left.cost_microusd == right.cost_microusd
}

fn validate_route_audit(
    stage: ExecutionRouteStage,
    decision: ExecutionRouteDecisionKind,
    mode: Option<ExecutionMode>,
    reason: ExecutionRouteReason,
    provenance: &ExecutionRouteProvenanceV1,
) -> Result<(), ExecutionPlanningContractError> {
    let route_matches_source = match provenance.source {
        ExecutionRouteSource::Classifier => {
            stage == ExecutionRouteStage::Initial
                && matches!(
                    (decision, mode, reason),
                    (
                        ExecutionRouteDecisionKind::NeedsInput,
                        None,
                        ExecutionRouteReason::PreflightInputMissing
                    ) | (
                        ExecutionRouteDecisionKind::Routed,
                        Some(ExecutionMode::Respond),
                        ExecutionRouteReason::SimpleResponse
                    ) | (
                        ExecutionRouteDecisionKind::Routed,
                        Some(ExecutionMode::Act),
                        ExecutionRouteReason::BoundedInteractiveWork
                    ) | (
                        ExecutionRouteDecisionKind::Routed,
                        Some(ExecutionMode::Run),
                        ExecutionRouteReason::ExplicitRun
                            | ExecutionRouteReason::BulkCollection
                            | ExecutionRouteReason::DurableOrResumable
                            | ExecutionRouteReason::HighFanout
                            | ExecutionRouteReason::ApprovalOrSignal
                    )
                )
        }
        ExecutionRouteSource::BlankObjective => {
            matches!(
                (stage, decision, mode, reason),
                (
                    ExecutionRouteStage::Initial,
                    ExecutionRouteDecisionKind::NeedsInput,
                    None,
                    ExecutionRouteReason::PreflightInputMissing
                )
            )
        }
        ExecutionRouteSource::SelectedExecutionTemplate => {
            matches!(
                (stage, decision, mode, reason),
                (
                    ExecutionRouteStage::Initial,
                    ExecutionRouteDecisionKind::Routed,
                    Some(ExecutionMode::Run),
                    ExecutionRouteReason::SelectedExecutionTemplate
                )
            )
        }
        ExecutionRouteSource::ActEscalation => {
            matches!(
                (stage, decision, mode, reason),
                (
                    ExecutionRouteStage::ActEscalation,
                    ExecutionRouteDecisionKind::Routed,
                    Some(ExecutionMode::Run),
                    ExecutionRouteReason::ActEscalation
                )
            )
        }
    };
    if !route_matches_source {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload".to_string(),
            message: "route stage, decision, mode, reason, and source are inconsistent".to_string(),
        });
    }

    validate_hash(
        "payload.provenance.objective_hash",
        &provenance.objective_hash,
    )?;
    if let Some(hash) = provenance.response_hash.as_deref() {
        validate_hash("payload.provenance.response_hash", hash)?;
    }
    if provenance
        .confidence_bps
        .is_some_and(|value| value > 10_000)
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.confidence_bps".to_string(),
            message: "must be within 0..=10000".to_string(),
        });
    }
    let needs_input = decision == ExecutionRouteDecisionKind::NeedsInput;
    if needs_input != (1..=8).contains(&provenance.missing_input_count) {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.missing_input_count".to_string(),
            message: "must contain 1..=8 only for needs-input decisions".to_string(),
        });
    }

    if provenance.source != ExecutionRouteSource::Classifier {
        if provenance.classifier_outcome != ExecutionRouteClassifierOutcome::NotCalled
            || provenance.provider_model.is_some()
            || provenance.prompt_version.is_some()
            || provenance.response_hash.is_some()
            || provenance.confidence_bps.is_some()
            || !provenance.usage.is_zero()
            || provenance.cost_microusd != 0
            || provenance.duration_micros != 0
        {
            return Err(ExecutionPlanningContractError::InvalidField {
                field: "payload.provenance".to_string(),
                message: "trusted routes cannot carry classifier evidence".to_string(),
            });
        }
        return Ok(());
    }

    if provenance.classifier_outcome == ExecutionRouteClassifierOutcome::NotCalled {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.classifier_outcome".to_string(),
            message: "classifier routes require an attempted-call outcome".to_string(),
        });
    }
    let provider_model = provenance.provider_model.as_deref().ok_or_else(|| {
        ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.provider_model".to_string(),
            message: "classifier routes require a provider model".to_string(),
        }
    })?;
    let prompt_version = provenance.prompt_version.as_deref().ok_or_else(|| {
        ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.prompt_version".to_string(),
            message: "classifier routes require a prompt version".to_string(),
        }
    })?;
    ensure_nonempty_bytes("payload.provenance.provider_model", provider_model, 128)?;
    ensure_nonempty_bytes("payload.provenance.prompt_version", prompt_version, 64)?;

    let collected = matches!(
        provenance.classifier_outcome,
        ExecutionRouteClassifierOutcome::Accepted
            | ExecutionRouteClassifierOutcome::Oversized
            | ExecutionRouteClassifierOutcome::SchemaRejected
            | ExecutionRouteClassifierOutcome::InvalidDecision
            | ExecutionRouteClassifierOutcome::LowConfidence
            | ExecutionRouteClassifierOutcome::ContextForcedAct
    );
    if collected != provenance.response_hash.is_some() {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.response_hash".to_string(),
            message: "response hash nullability does not match classifier outcome".to_string(),
        });
    }
    if !collected && (!provenance.usage.is_zero() || provenance.cost_microusd != 0) {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.usage".to_string(),
            message: "calls without a collected response cannot carry usage or cost".to_string(),
        });
    }
    let parsed = matches!(
        provenance.classifier_outcome,
        ExecutionRouteClassifierOutcome::Accepted
            | ExecutionRouteClassifierOutcome::LowConfidence
            | ExecutionRouteClassifierOutcome::ContextForcedAct
    );
    if parsed != provenance.confidence_bps.is_some()
        || (provenance.classifier_outcome == ExecutionRouteClassifierOutcome::InvalidDecision
            && provenance.confidence_bps.is_some())
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.confidence_bps".to_string(),
            message: "confidence nullability does not match classifier outcome".to_string(),
        });
    }
    if provenance.classifier_outcome != ExecutionRouteClassifierOutcome::Accepted
        && !matches!(
            (decision, mode, reason),
            (
                ExecutionRouteDecisionKind::Routed,
                Some(ExecutionMode::Act),
                ExecutionRouteReason::BoundedInteractiveWork
            )
        )
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "payload.provenance.classifier_outcome".to_string(),
            message: "non-accepted classifier outcomes must conservatively route to Act"
                .to_string(),
        });
    }
    Ok(())
}

/// Sorts, bounds, and hashes a complete violation vector for one audit report.
pub fn bounded_audit_report(
    compiler: bool,
    mut violations: Vec<ExecutionAuditViolationV1>,
) -> Result<ExecutionAuditReportV1, ExecutionPlanningContractError> {
    for violation in &violations {
        ensure_bytes("violation.code", &violation.code, 64)?;
        ensure_bytes("violation.path", &violation.path, 512)?;
        ensure_bytes("violation.message", &violation.message, 512)?;
    }
    violations.sort();
    let preimage = canonical_json_bytes(&serde_json::json!({
        "schema_version": 1,
        "violations": violations,
    }))?;
    let domain = if compiler {
        "moa.execution.compiler-report.v1"
    } else {
        "moa.execution.schema-report.v1"
    };
    let full_report_hash = execution_planning_hash(domain, &preimage);
    let omitted = violations
        .len()
        .saturating_sub(EXECUTION_AUDIT_MAX_VIOLATIONS);
    violations.truncate(EXECUTION_AUDIT_MAX_VIOLATIONS);
    let omitted_violations =
        u32::try_from(omitted).map_err(|_| ExecutionPlanningContractError::BoundExceeded {
            field: "omitted_violations".to_string(),
            limit: u32::MAX as usize,
            observed: omitted,
        })?;
    Ok(if compiler {
        ExecutionAuditReportV1::Compiler {
            violations,
            omitted_violations,
            full_report_hash,
        }
    } else {
        ExecutionAuditReportV1::Schema {
            violations,
            omitted_violations,
            full_report_hash,
        }
    })
}

fn ensure_bytes(
    field: &str,
    value: &str,
    limit: usize,
) -> Result<(), ExecutionPlanningContractError> {
    if value.len() > limit {
        return Err(ExecutionPlanningContractError::BoundExceeded {
            field: field.to_string(),
            limit,
            observed: value.len(),
        });
    }
    Ok(())
}

fn ensure_nonempty_bytes(
    field: &str,
    value: &str,
    limit: usize,
) -> Result<(), ExecutionPlanningContractError> {
    if value.is_empty() {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: field.to_string(),
            message: "must be non-empty".to_string(),
        });
    }
    ensure_bytes(field, value, limit)
}

fn validate_hash(field: &str, value: &str) -> Result<(), ExecutionPlanningContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: field.to_string(),
            message: "must be 64 lowercase hexadecimal characters".to_string(),
        });
    }
    Ok(())
}

/// Serializes a value using MOA's canonical JSON formatter.
pub fn canonical_json_bytes<T: Serialize>(
    value: &T,
) -> Result<Vec<u8>, ExecutionPlanningContractError> {
    let mut serializer =
        serde_json::Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
    value
        .serialize(&mut serializer)
        .map_err(|error| ExecutionPlanningContractError::Json(error.to_string()))?;
    Ok(serializer.into_inner())
}

fn canonical_enum_string<T: Serialize>(
    value: &T,
) -> Result<String, ExecutionPlanningContractError> {
    let value = serde_json::to_value(value)
        .map_err(|error| ExecutionPlanningContractError::Json(error.to_string()))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExecutionPlanningContractError::Json("enum was not a string".to_string()))
}

fn validate_canonical_document(
    field: &str,
    document: &str,
) -> Result<Value, ExecutionPlanningContractError> {
    let value: Value = serde_json::from_str(document).map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: field.to_string(),
            message: format!("must be valid JSON: {error}"),
        }
    })?;
    let canonical = canonical_json_bytes(&value)?;
    if canonical != document.as_bytes() {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: field.to_string(),
            message: "must contain canonical JSON bytes".to_string(),
        });
    }
    Ok(value)
}

fn validate_canonical_report(
    field: &str,
    document: &str,
) -> Result<ExecutionAuditReportV1, ExecutionPlanningContractError> {
    let value = validate_canonical_document(field, document)?;
    serde_json::from_value(value).map_err(|error| ExecutionPlanningContractError::InvalidField {
        field: field.to_string(),
        message: format!("must match ExecutionAuditReportV1: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted_classifier_provenance() -> ExecutionRouteProvenanceV1 {
        ExecutionRouteProvenanceV1 {
            source: ExecutionRouteSource::Classifier,
            classifier_outcome: ExecutionRouteClassifierOutcome::Accepted,
            provider_model: Some("route-model".to_string()),
            prompt_version: Some("execution-router-v1".to_string()),
            objective_hash: "a".repeat(64),
            response_hash: Some("b".repeat(64)),
            confidence_bps: Some(9_500),
            missing_input_count: 0,
            usage: ExecutionRouteUsageV1::default(),
            cost_microusd: 0,
            duration_micros: 1,
        }
    }

    fn planner_call_envelope(
        outcome: ExecutionPlannerOutcome,
        candidate_hash: Option<String>,
        candidate_json: Option<String>,
        compiler_report: Option<String>,
    ) -> ExecutionPlanningAuditEnvelopeV1 {
        ExecutionPlanningAuditEnvelopeV1 {
            schema_version: 1,
            tenant_id: TenantId::from(Uuid::nil()),
            contact_id: None,
            session_id: Some(SessionId(Uuid::nil())),
            originating_sequence: Some(1),
            payload: ExecutionPlanningAuditPayloadV1::PlannerCall {
                call_kind: ExecutionPlannerCallKind::InitialPlan,
                call_ordinal: 0,
                run_uid: None,
                plan_revision: None,
                outcome,
                provider_model: "planner-model".to_string(),
                prompt_version: "execution-planner-v1".to_string(),
                candidate_hash,
                candidate_json,
                compiler_report,
                duration_micros: 1,
                created_at: Utc::now(),
            },
        }
    }

    fn canonical_report(report: &ExecutionAuditReportV1) -> String {
        String::from_utf8(canonical_json_bytes(report).expect("canonicalize audit report"))
            .expect("canonical audit report should be UTF-8")
    }

    fn compile_envelope(
        source: ExecutionCompileSource,
        session_bound: bool,
    ) -> ExecutionPlanningAuditEnvelopeV1 {
        let report = canonical_report(
            &bounded_audit_report(true, Vec::new()).expect("empty compiler report"),
        );
        ExecutionPlanningAuditEnvelopeV1 {
            schema_version: 1,
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            contact_id: None,
            session_id: session_bound.then(|| SessionId(Uuid::from_u128(2))),
            originating_sequence: session_bound.then_some(3),
            payload: ExecutionPlanningAuditPayloadV1::Compile {
                source,
                operation_key: match source {
                    ExecutionCompileSource::ExperimentTemplate => format!(
                        "experiment:{}:{}:none",
                        Uuid::from_u128(4),
                        Uuid::from_u128(5)
                    ),
                    ExecutionCompileSource::SkillRegression => {
                        format!("skill_regression:{}:{}", Uuid::from_u128(6), "a".repeat(64))
                    }
                    _ => "compile-operation".to_string(),
                },
                run_uid: None,
                plan_revision: None,
                outcome: ExecutionCompileOutcome::Accepted,
                candidate_hash: "b".repeat(64),
                final_plan_hash: Some("c".repeat(64)),
                validation_report: report,
                duration_micros: 1,
                created_at: Utc::now(),
            },
        }
    }

    #[test]
    fn execution_run_started_enforces_confirmation_shape() {
        // Pins: queued and awaiting-confirmation admissions cannot exchange evidence shapes.
        let queued = ExecutionRunStarted {
            run_uid: Uuid::nil(),
            originating_user_sequence_num: 7,
            plan_revision: 1,
            status: ExecutionRunAdmissionStatus::Queued,
            confirmation: None,
        };
        assert_eq!(queued.validate(), Ok(()));

        let mut invalid = queued.clone();
        invalid.status = ExecutionRunAdmissionStatus::AwaitingConfirmation;
        assert!(matches!(
            invalid.validate(),
            Err(ExecutionPlanningContractError::InvalidField { field, .. }) if field == "confirmation"
        ));
    }

    #[test]
    fn execution_source_provenance_rejects_cross_cohort_fields_and_hash_drift() {
        // Pins: generated and skill-template source cohorts stay closed and plan-hash bound.
        let hash = "a".repeat(64);
        let provenance = ExecutionSourceProvenanceV1::GeneratedPlan {
            route_reason: ExecutionRouteReason::ExplicitRun,
            planner: GeneratedPlanPlannerProvenanceV1 {
                model: "planner-model".to_string(),
                prompt_version: "execution-planner-v1".to_string(),
                candidate_hash: "b".repeat(64),
                compiler_report_hash: "c".repeat(64),
                final_plan_hash: hash.clone(),
                repair_attempts: 0,
            },
        };
        assert_eq!(provenance.validate(&hash), Ok(()));
        assert!(provenance.validate(&"d".repeat(64)).is_err());

        let skill = ExecutionSourceProvenanceV1::SkillTemplate {
            route_reason: ExecutionRouteReason::SelectedExecutionTemplate,
            skill_template_ref: "skill://durable-report".to_string(),
            skill_template_revision_uid: Uuid::from_u128(1),
        };
        assert_eq!(skill.validate(&hash), Ok(()));
        let wrong_skill_route = ExecutionSourceProvenanceV1::SkillTemplate {
            route_reason: ExecutionRouteReason::ExplicitRun,
            skill_template_ref: "skill://durable-report".to_string(),
            skill_template_revision_uid: Uuid::from_u128(1),
        };
        assert!(matches!(
            wrong_skill_route.validate(&hash),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "route_reason"
        ));
    }

    #[test]
    fn experiment_template_source_provenance_has_exact_uuid_and_null_serde_shape() {
        // Pins: run provenance carries every experiment identity and writes an explicit null
        // trial field, while trial provenance writes its exact UUID and no extra keys.
        let run = ExecutionSourceProvenanceV1::ExperimentTemplate {
            route_reason: ExecutionRouteReason::ExplicitRun,
            skill_template_ref: "skill://durable-report".to_string(),
            skill_template_revision_uid: Uuid::from_u128(1),
            experiment_run_uid: Uuid::from_u128(2),
            score_run_id: Uuid::from_u128(3),
            trial_uid: None,
        };
        let run_json = serde_json::json!({
            "kind": "experiment_template",
            "route_reason": "explicit_run",
            "skill_template_ref": "skill://durable-report",
            "skill_template_revision_uid": "00000000-0000-0000-0000-000000000001",
            "experiment_run_uid": "00000000-0000-0000-0000-000000000002",
            "score_run_id": "00000000-0000-0000-0000-000000000003",
            "trial_uid": null,
        });
        assert_eq!(
            serde_json::to_value(&run).expect("run provenance should serialize"),
            run_json
        );
        assert_eq!(
            serde_json::from_value::<ExecutionSourceProvenanceV1>(run_json.clone())
                .expect("explicit-null run provenance should deserialize"),
            run
        );

        let trial_uid = Uuid::from_u128(4);
        let trial = ExecutionSourceProvenanceV1::ExperimentTemplate {
            route_reason: ExecutionRouteReason::ExplicitRun,
            skill_template_ref: "skill://durable-report".to_string(),
            skill_template_revision_uid: Uuid::from_u128(1),
            experiment_run_uid: Uuid::from_u128(2),
            score_run_id: Uuid::from_u128(3),
            trial_uid: Some(trial_uid),
        };
        let mut trial_json = run_json;
        trial_json["trial_uid"] = serde_json::json!(trial_uid);
        assert_eq!(
            serde_json::to_value(&trial).expect("trial provenance should serialize"),
            trial_json
        );
        assert_eq!(
            serde_json::from_value::<ExecutionSourceProvenanceV1>(trial_json)
                .expect("UUID-valued trial provenance should deserialize"),
            trial
        );
    }

    #[test]
    fn experiment_template_source_provenance_rejects_shape_and_route_drift() {
        // Pins: the experiment cohort cannot omit explicit trial nullability, add a reserved
        // cross-cohort field, use the skill route, or carry an empty template reference.
        let hash = "a".repeat(64);
        let valid = ExecutionSourceProvenanceV1::ExperimentTemplate {
            route_reason: ExecutionRouteReason::ExplicitRun,
            skill_template_ref: "skill://durable-report".to_string(),
            skill_template_revision_uid: Uuid::from_u128(1),
            experiment_run_uid: Uuid::from_u128(2),
            score_run_id: Uuid::from_u128(3),
            trial_uid: None,
        };
        assert_eq!(valid.validate(&hash), Ok(()));

        let mut missing_trial =
            serde_json::to_value(&valid).expect("valid provenance should serialize");
        missing_trial
            .as_object_mut()
            .expect("provenance should be an object")
            .remove("trial_uid");
        assert!(
            serde_json::from_value::<ExecutionSourceProvenanceV1>(missing_trial).is_err(),
            "trial_uid must be explicitly present as a UUID or null"
        );

        let mut cross_cohort =
            serde_json::to_value(&valid).expect("valid provenance should serialize");
        cross_cohort["planner"] = serde_json::json!({});
        assert!(
            serde_json::from_value::<ExecutionSourceProvenanceV1>(cross_cohort).is_err(),
            "experiment provenance must reject reserved cross-cohort fields"
        );

        let wrong_route = ExecutionSourceProvenanceV1::ExperimentTemplate {
            route_reason: ExecutionRouteReason::SelectedExecutionTemplate,
            skill_template_ref: "skill://durable-report".to_string(),
            skill_template_revision_uid: Uuid::from_u128(1),
            experiment_run_uid: Uuid::from_u128(2),
            score_run_id: Uuid::from_u128(3),
            trial_uid: None,
        };
        assert!(matches!(
            wrong_route.validate(&hash),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "route_reason"
        ));

        let empty_ref = ExecutionSourceProvenanceV1::ExperimentTemplate {
            route_reason: ExecutionRouteReason::ExplicitRun,
            skill_template_ref: String::new(),
            skill_template_revision_uid: Uuid::from_u128(1),
            experiment_run_uid: Uuid::from_u128(2),
            score_run_id: Uuid::from_u128(3),
            trial_uid: None,
        };
        assert!(matches!(
            empty_ref.validate(&hash),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "skill_template_ref"
        ));
    }

    #[test]
    fn execution_planning_planner_call_nullability_matrix_is_strict() {
        // Pins: every planner outcome retains exactly the candidate/report cohort allowed by Task 7.
        let candidate_json = "{}".to_string();
        let candidate_hash = execution_planning_hash(
            "moa.execution.planner-candidate.v1",
            candidate_json.as_bytes(),
        );
        let compiler_report = canonical_report(
            &bounded_audit_report(true, Vec::new()).expect("empty compiler report"),
        );
        let schema_report = canonical_report(
            &bounded_audit_report(
                false,
                vec![ExecutionAuditViolationV1 {
                    code: "schema".to_string(),
                    path: "/".to_string(),
                    message: "invalid".to_string(),
                }],
            )
            .expect("schema report"),
        );
        let raw_hash = execution_planning_hash(
            "moa.execution.planner-response.v1",
            b"invalid provider response",
        );
        let oversized_report = canonical_report(&ExecutionAuditReportV1::Oversized {
            field: ExecutionOversizedAuditField::Candidate,
            limit_bytes: EXECUTION_CANDIDATE_MAX_BYTES as u64,
            observed_bytes: EXECUTION_CANDIDATE_MAX_BYTES as u64 + 1,
            content_hash: execution_planning_hash(
                "moa.execution.oversized-content.v1",
                b"oversized provider response",
            ),
        });

        for outcome in [
            ExecutionPlannerOutcome::Accepted,
            ExecutionPlannerOutcome::NeedsInput,
            ExecutionPlannerOutcome::Unsupported,
            ExecutionPlannerOutcome::CompilerRejected,
        ] {
            let envelope = planner_call_envelope(
                outcome,
                Some(candidate_hash.clone()),
                Some(candidate_json.clone()),
                Some(compiler_report.clone()),
            );
            assert_eq!(
                validate_planning_audit_envelope(&envelope),
                Ok(()),
                "{outcome:?} should retain canonical candidate and compiler report"
            );
        }

        let mut immutable = planner_call_envelope(
            ExecutionPlannerOutcome::ImmutableGoalChanged,
            Some(candidate_hash.clone()),
            Some(candidate_json.clone()),
            Some(schema_report.clone()),
        );
        if let ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind,
            call_ordinal,
            ..
        } = &mut immutable.payload
        {
            *call_kind = ExecutionPlannerCallKind::InitialRepair;
            *call_ordinal = 1;
        }
        assert_eq!(validate_planning_audit_envelope(&immutable), Ok(()));

        assert_eq!(
            validate_planning_audit_envelope(&planner_call_envelope(
                ExecutionPlannerOutcome::SchemaRejected,
                Some(raw_hash.clone()),
                None,
                Some(schema_report),
            )),
            Ok(())
        );
        assert_eq!(
            validate_planning_audit_envelope(&planner_call_envelope(
                ExecutionPlannerOutcome::Oversized,
                Some(raw_hash),
                None,
                Some(oversized_report),
            )),
            Ok(())
        );
        assert_eq!(
            validate_planning_audit_envelope(&planner_call_envelope(
                ExecutionPlannerOutcome::ProviderError,
                None,
                None,
                None,
            )),
            Ok(())
        );

        let accepted_without_report = planner_call_envelope(
            ExecutionPlannerOutcome::Accepted,
            Some(candidate_hash),
            Some(candidate_json),
            None,
        );
        assert!(matches!(
            validate_planning_audit_envelope(&accepted_without_report),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "payload.compiler_report"
        ));
    }

    #[test]
    fn execution_compile_audit_destination_is_source_owned() {
        // Pins: experiment template compiles retain their Session origin, while background skill
        // regression compiles remain the sole sessionless compile cohort.
        let experiment = compile_envelope(ExecutionCompileSource::ExperimentTemplate, true);
        assert_eq!(validate_planning_audit_envelope(&experiment), Ok(()));

        let sessionless_experiment =
            compile_envelope(ExecutionCompileSource::ExperimentTemplate, false);
        assert!(matches!(
            validate_planning_audit_envelope(&sessionless_experiment),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "session_id"
        ));

        let regression = compile_envelope(ExecutionCompileSource::SkillRegression, false);
        assert_eq!(validate_planning_audit_envelope(&regression), Ok(()));

        let session_bound_regression =
            compile_envelope(ExecutionCompileSource::SkillRegression, true);
        assert!(matches!(
            validate_planning_audit_envelope(&session_bound_regression),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "session_id"
        ));

        let mut amendment = compile_envelope(ExecutionCompileSource::Amendment, true);
        if let ExecutionPlanningAuditPayloadV1::Compile {
            operation_key,
            run_uid,
            plan_revision,
            candidate_hash,
            ..
        } = &mut amendment.payload
        {
            let uid = Uuid::from_u128(7);
            *run_uid = Some(uid);
            *plan_revision = Some(8);
            *operation_key = format!("run:{uid}:8:amendment:{candidate_hash}");
        }
        assert_eq!(validate_planning_audit_envelope(&amendment), Ok(()));

        if let ExecutionPlanningAuditPayloadV1::Compile { operation_key, .. } =
            &mut amendment.payload
        {
            *operation_key = "run:wrong:8:amendment:hash".to_string();
        }
        assert!(matches!(
            validate_planning_audit_envelope(&amendment),
            Err(ExecutionPlanningContractError::InvalidField { field, .. })
                if field == "payload.operation_key"
        ));
    }

    #[test]
    fn execution_route_and_planning_audit_semantic_replay_ignores_only_measurements() {
        // Pins: commit-before-result replay preserves the first timestamp and duration only.
        let tenant_id = TenantId::from(Uuid::nil());
        let mut first = ExecutionPlanningAuditEnvelopeV1 {
            schema_version: 1,
            tenant_id,
            contact_id: None,
            session_id: None,
            originating_sequence: None,
            payload: ExecutionPlanningAuditPayloadV1::Route {
                stage: ExecutionRouteStage::Initial,
                decision: ExecutionRouteDecisionKind::Routed,
                mode: Some(ExecutionMode::Respond),
                reason: ExecutionRouteReason::SimpleResponse,
                provenance: accepted_classifier_provenance(),
                accepted_at: Utc::now(),
            },
        };
        let mut replay = first.clone();
        if let ExecutionPlanningAuditPayloadV1::Route { accepted_at, .. } = &mut replay.payload {
            *accepted_at += chrono::Duration::seconds(1);
        }
        if let ExecutionPlanningAuditPayloadV1::Route { provenance, .. } = &mut replay.payload {
            provenance.duration_micros += 1;
        }
        assert!(planning_audit_semantically_equal(&first, &replay));
        if let ExecutionPlanningAuditPayloadV1::Route { reason, .. } = &mut first.payload {
            *reason = ExecutionRouteReason::BoundedInteractiveWork;
        }
        assert!(!planning_audit_semantically_equal(&first, &replay));

        let candidate_json = "{}".to_string();
        let candidate_hash = execution_planning_hash(
            "moa.execution.planner-candidate.v1",
            candidate_json.as_bytes(),
        );
        let compiler_report = canonical_report(
            &bounded_audit_report(true, Vec::new()).expect("empty compiler report"),
        );
        let accepted = planner_call_envelope(
            ExecutionPlannerOutcome::Accepted,
            Some(candidate_hash),
            Some(candidate_json),
            Some(compiler_report),
        );
        let mut accepted_replay = accepted.clone();
        if let ExecutionPlanningAuditPayloadV1::PlannerCall {
            duration_micros,
            created_at,
            ..
        } = &mut accepted_replay.payload
        {
            *duration_micros += 1;
            *created_at += chrono::Duration::seconds(1);
        }
        assert!(planning_audit_semantically_equal(
            &accepted,
            &accepted_replay
        ));
        if let ExecutionPlanningAuditPayloadV1::PlannerCall {
            compiler_report, ..
        } = &mut accepted_replay.payload
        {
            *compiler_report = Some(canonical_report(
                &bounded_audit_report(
                    true,
                    vec![ExecutionAuditViolationV1 {
                        code: "changed".to_string(),
                        path: "/plan".to_string(),
                        message: "different report".to_string(),
                    }],
                )
                .expect("changed compiler report"),
            ));
        }
        assert!(!planning_audit_semantically_equal(
            &accepted,
            &accepted_replay
        ));
    }

    #[test]
    fn execution_planning_audit_report_sorts_truncates_and_hashes_full_input() {
        // Pins: bounded audit evidence remains deterministic without dropping full-report identity.
        let violations = (0..300)
            .rev()
            .map(|index| ExecutionAuditViolationV1 {
                code: format!("code-{index:03}"),
                path: format!("/nodes/{index}"),
                message: "invalid".to_string(),
            })
            .collect();
        let report = bounded_audit_report(true, violations).expect("bound compiler report");
        let ExecutionAuditReportV1::Compiler {
            violations,
            omitted_violations,
            full_report_hash,
        } = report
        else {
            panic!("expected compiler report");
        };
        assert_eq!(violations.len(), 256);
        assert_eq!(omitted_violations, 44);
        assert_eq!(full_report_hash.len(), 64);
        assert!(violations.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn execution_planning_audit_dedupe_key_pins_nullable_framing() {
        // Pins: null and empty tuple fields cannot collide in durable planning-audit dedupe keys.
        let mut envelope = ExecutionPlanningAuditEnvelopeV1 {
            schema_version: 1,
            tenant_id: TenantId::from(Uuid::nil()),
            contact_id: None,
            session_id: Some(SessionId(Uuid::nil())),
            originating_sequence: Some(1),
            payload: ExecutionPlanningAuditPayloadV1::Route {
                stage: ExecutionRouteStage::Initial,
                decision: ExecutionRouteDecisionKind::Routed,
                mode: Some(ExecutionMode::Respond),
                reason: ExecutionRouteReason::SimpleResponse,
                provenance: accepted_classifier_provenance(),
                accepted_at: Utc::now(),
            },
        };
        let null_key = execution_planning_dedupe_key(&envelope).expect("route key");
        envelope.contact_id = Some(ContactId(Uuid::nil()));
        let empty_key = execution_planning_dedupe_key(&envelope).expect("contact route key");
        assert_ne!(null_key, empty_key);
        assert!(null_key.starts_with("execution-planning-v1:"));
        assert_eq!(null_key.len(), "execution-planning-v1:".len() + 64);
    }
}
