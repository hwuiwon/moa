//! Normalized planning-audit identities, labels, and row codecs.

use super::audit::{CompileAuditEvidence, PlannerCallAuditEvidence, RouteAuditEvidence};
use super::rows::{optional_u64, required_u64};
use super::*;

pub(super) const fn route_source_label(source: ExecutionRouteSource) -> &'static str {
    match source {
        ExecutionRouteSource::Classifier => "classifier",
        ExecutionRouteSource::BlankObjective => "blank_objective",
        ExecutionRouteSource::SelectedExecutionTemplate => "selected_execution_template",
        ExecutionRouteSource::DurableUpgrade => "durable_upgrade",
    }
}

pub(super) fn route_source_from_str(value: &str) -> Result<ExecutionRouteSource> {
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

pub(super) const fn route_classifier_outcome_label(
    outcome: ExecutionRouteClassifierOutcome,
) -> &'static str {
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

pub(super) fn route_classifier_outcome_from_str(
    value: &str,
) -> Result<ExecutionRouteClassifierOutcome> {
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
pub(super) struct PersistedRouteAudit {
    pub(super) audit_uid: Uuid,
    pub(super) stage: ExecutionRouteStage,
    pub(super) evidence: RouteAuditEvidence,
}

#[derive(Clone, Debug)]
pub(super) struct PersistedPlannerAudit {
    audit_uid: Uuid,
    call_ordinal: u8,
    run_uid: Option<Uuid>,
    plan_revision: Option<u64>,
    provider_model: String,
    prompt_version: String,
    candidate_json: Option<String>,
    compiler_report: Option<String>,
    pub(super) evidence: PlannerCallAuditEvidence,
}

impl PersistedPlannerAudit {
    pub(super) fn semantically_matches(
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
pub(super) struct PersistedCompileAudit {
    audit_uid: Uuid,
    session_id: Option<SessionId>,
    originating_sequence: Option<u64>,
    run_uid: Option<Uuid>,
    plan_revision: Option<u64>,
    operation_key: String,
    validation_report: String,
    pub(super) evidence: CompileAuditEvidence,
}

impl PersistedCompileAudit {
    pub(super) fn semantically_matches(
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

pub(super) fn validate_audit_scope(
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

pub(super) fn route_audit_uid(
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
pub(super) fn planner_audit_uid(
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

pub(super) fn compile_audit_uid(
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

pub(super) fn execution_audit_uid(domain: &str, fields: &[Option<String>]) -> Result<Uuid> {
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

pub(super) fn route_audit_from_row(row: &PgRow) -> Result<PersistedRouteAudit> {
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

pub(super) fn planner_audit_from_row(row: &PgRow) -> Result<PersistedPlannerAudit> {
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

pub(super) fn compile_audit_from_row(row: &PgRow) -> Result<PersistedCompileAudit> {
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

pub(super) const fn route_stage_label(stage: ExecutionRouteStage) -> &'static str {
    match stage {
        ExecutionRouteStage::Initial => "initial",
        ExecutionRouteStage::DurableUpgrade => "durable_upgrade",
    }
}

pub(super) fn route_stage_from_str(value: &str) -> Result<ExecutionRouteStage> {
    match value {
        "initial" => Ok(ExecutionRouteStage::Initial),
        "durable_upgrade" => Ok(ExecutionRouteStage::DurableUpgrade),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution route stage `{value}`"),
        }),
    }
}

pub(super) const fn route_decision_label(decision: ExecutionRouteKind) -> &'static str {
    match decision {
        ExecutionRouteKind::Respond => "respond",
        ExecutionRouteKind::Execute => "execute",
        ExecutionRouteKind::NeedsInput => "needs_input",
    }
}

pub(super) fn route_decision_from_str(value: &str) -> Result<ExecutionRouteKind> {
    match value {
        "respond" => Ok(ExecutionRouteKind::Respond),
        "execute" => Ok(ExecutionRouteKind::Execute),
        "needs_input" => Ok(ExecutionRouteKind::NeedsInput),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution route decision `{value}`"),
        }),
    }
}

pub(super) const fn execution_strategy_label(strategy: ExecutionStrategy) -> &'static str {
    match strategy {
        ExecutionStrategy::Inline => "inline",
        ExecutionStrategy::Durable => "durable",
    }
}

pub(super) fn execution_strategy_from_str(value: &str) -> Result<ExecutionStrategy> {
    match value {
        "inline" => Ok(ExecutionStrategy::Inline),
        "durable" => Ok(ExecutionStrategy::Durable),
        _ => Err(Error::InvalidRepositoryData {
            message: format!("unknown execution strategy `{value}`"),
        }),
    }
}

pub(super) const fn planner_call_label(call: ExecutionPlannerCallKind) -> &'static str {
    match call {
        ExecutionPlannerCallKind::InitialPlan => "initial_plan",
        ExecutionPlannerCallKind::InitialRepair => "initial_repair",
        ExecutionPlannerCallKind::Amendment => "amendment",
        ExecutionPlannerCallKind::AmendmentRepair => "amendment_repair",
    }
}

pub(super) fn planner_call_from_str(value: &str) -> Result<ExecutionPlannerCallKind> {
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

pub(super) const fn planner_outcome_label(outcome: ExecutionPlannerOutcome) -> &'static str {
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

pub(super) fn planner_outcome_from_str(value: &str) -> Result<ExecutionPlannerOutcome> {
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

pub(super) const fn compile_source_label(source: ExecutionCompileSource) -> &'static str {
    match source {
        ExecutionCompileSource::GeneratedPlan => "generated_plan",
        ExecutionCompileSource::SkillTemplate => "skill_template",
        ExecutionCompileSource::ExperimentTemplate => "experiment_template",
        ExecutionCompileSource::Amendment => "amendment",
        ExecutionCompileSource::SkillRegression => "skill_regression",
    }
}

pub(super) fn compile_source_from_str(value: &str) -> Result<ExecutionCompileSource> {
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

pub(super) const fn compile_outcome_label(outcome: ExecutionCompileOutcome) -> &'static str {
    match outcome {
        ExecutionCompileOutcome::Accepted => "accepted",
        ExecutionCompileOutcome::NeedsInput => "needs_input",
        ExecutionCompileOutcome::Unsupported => "unsupported",
        ExecutionCompileOutcome::Rejected => "rejected",
    }
}

pub(super) fn compile_outcome_from_str(value: &str) -> Result<ExecutionCompileOutcome> {
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
