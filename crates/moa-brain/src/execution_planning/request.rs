//! Frozen planner inputs and strict provider request construction.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{GeneratedAmendmentCandidate, GeneratedExecutionCandidate};
use moa_core::{
    config::ExecutionConfig,
    types::{
        completion::{CompletionRequest, JsonResponseFormat, NativeWebSearchPolicy},
        context::ContextMessage,
        execution_planning::{ActEscalationSignal, ExecutionTemplateInvocation},
        identifiers::ModelId,
    },
};
use moa_execution::{
    compiler::CanonicalExecutionPlan,
    repository::{CompileAuditWriteOutcome, PlannerCallAuditWriteOutcome},
    state::{ExecutionProjection, ExecutionTaskId},
    wire::ExecutionPlanningContextSnapshotV1,
};
use moa_observability::{record_execution_compile_duration, record_execution_planner_call};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable v1 execution-planner prompt identifier.
pub const EXECUTION_PLANNER_PROMPT_VERSION: &str = "execution-planner-v1";
/// Fixed maximum collected planner output tokens.
pub const EXECUTION_PLANNER_MAX_OUTPUT_TOKENS: usize = 32_768;
const EXECUTION_PLANNER_PROMPT: &str = include_str!("../prompts/execution_planner.txt");

/// Immutable inputs for initial plan generation or exact template instantiation.
#[derive(Clone, Debug)]
pub struct ExecutionPlanningRequest {
    /// Byte-identical persisted user-message text.
    pub objective: String,
    /// Immutable session-derived planning authority and capability snapshot.
    pub context: ExecutionPlanningContextSnapshotV1,
    /// Explicit exact template invocation, when present.
    pub execution_template: Option<ExecutionTemplateInvocation>,
    /// Bounded evidence from an Act-to-Run escalation.
    pub escalation: Option<ActEscalationSignal>,
    /// Auxiliary provider model selected by server configuration.
    pub planner_model: ModelId,
    /// Tenant-independent compiler and estimate settings.
    pub config: ExecutionConfig,
    /// Journaled planning/compile time.
    pub now: DateTime<Utc>,
}

/// Frozen evidence supplied to one amendment planner operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningEvidence {
    /// Immutable original run goal.
    pub goal: moa_artifacts::execution_plan::ExecutionGoalContract,
    /// Active immutable plan snapshot.
    pub active_plan: CanonicalExecutionPlan,
    /// Current durable run projection and completed structured outputs.
    pub projection: ExecutionProjection,
    /// Structured failure evidence that caused WaitingReplan.
    pub failure_evidence: Value,
    /// Exact originating waiting task.
    pub waiting_task: ExecutionTaskId,
}

/// Immutable inputs for one revision-fenced amendment generation operation.
#[derive(Clone, Debug)]
pub struct ExecutionAmendmentPlanningRequest {
    /// Run whose plan is waiting for amendment.
    pub run_uid: uuid::Uuid,
    /// Active base plan revision.
    pub base_plan_revision: u64,
    /// Persisted original planning context, optionally narrowed for live revocation.
    pub context: ExecutionPlanningContextSnapshotV1,
    /// Immutable goal and run evidence.
    pub evidence: AmendmentPlanningEvidence,
    /// Resource envelope remaining before amendment acceptance.
    pub remaining_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
    /// Auxiliary provider model.
    pub planner_model: ModelId,
    /// Compiler settings.
    pub config: ExecutionConfig,
    /// Journaled planner time.
    pub now: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenInitialPrompt<'a> {
    schema_version: u8,
    objective: &'a str,
    catalog: &'a moa_execution::ExecutionCapabilityCatalog,
    authorization: &'a moa_execution::ExecutionAuthorizationEnvelope,
    pinned_instruction_skills: &'a [moa_execution::wire::PinnedInstructionSkill],
    execution_templates: &'a [moa_execution::wire::PinnedExecutionTemplate],
    budget: &'a moa_artifacts::execution_plan::ExecutionBudgetLimit,
    escalation: Option<&'a ActEscalationSignal>,
}

/// Constructs the exact no-tools, no-native-search strict initial planner request.
pub fn initial_completion_request(
    request: &ExecutionPlanningRequest,
) -> Result<CompletionRequest, serde_json::Error> {
    let frozen = FrozenInitialPrompt {
        schema_version: 1,
        objective: &request.objective,
        catalog: &request.context.catalog,
        authorization: &request.context.authorization,
        pinned_instruction_skills: &request.context.pinned_instruction_skills,
        execution_templates: &request.context.execution_templates,
        budget: &request.context.budget,
        escalation: request.escalation.as_ref(),
    };
    strict_request::<GeneratedExecutionCandidate>(
        request.planner_model.clone(),
        "generated_execution_candidate_v1",
        "Generate one strict immutable execution goal and supported execution plan.",
        format!(
            "{EXECUTION_PLANNER_PROMPT}\n\n<frozen_planning_context>{}</frozen_planning_context>",
            serde_json::to_string(&frozen)?
        ),
    )
}

/// Constructs the sole permitted initial-plan repair request from frozen first-call inputs.
pub fn initial_repair_completion_request(
    request: &ExecutionPlanningRequest,
    original_candidate_json: &str,
    immutable_goal_json: &str,
    compiler_report_json: &str,
) -> Result<CompletionRequest, serde_json::Error> {
    let mut completion = initial_completion_request(request)?;
    completion.messages.push(ContextMessage::user(format!(
        "Repair the candidate exactly once. Preserve immutable_goal byte-for-byte after canonicalization. Do not discover new authority or capabilities.\n<original_candidate>{original_candidate_json}</original_candidate>\n<immutable_goal>{immutable_goal_json}</immutable_goal>\n<compiler_report>{compiler_report_json}</compiler_report>"
    )));
    Ok(completion)
}

/// Constructs a strict amendment request over persisted, revision-fenced run state.
pub fn amendment_completion_request(
    request: &ExecutionAmendmentPlanningRequest,
    repair: Option<(&str, &str)>,
) -> Result<CompletionRequest, serde_json::Error> {
    #[derive(Serialize)]
    struct FrozenAmendmentPrompt<'a> {
        schema_version: u8,
        run_uid: uuid::Uuid,
        base_plan_revision: u64,
        goal: &'a moa_artifacts::execution_plan::ExecutionGoalContract,
        evidence: &'a AmendmentPlanningEvidence,
        catalog: &'a moa_execution::ExecutionCapabilityCatalog,
        authorization: &'a moa_execution::ExecutionAuthorizationEnvelope,
        remaining_budget: &'a moa_artifacts::execution_plan::ExecutionBudgetLimit,
    }
    let frozen = FrozenAmendmentPrompt {
        schema_version: 1,
        run_uid: request.run_uid,
        base_plan_revision: request.base_plan_revision,
        goal: &request.evidence.goal,
        evidence: &request.evidence,
        catalog: &request.context.catalog,
        authorization: &request.context.authorization,
        remaining_budget: &request.remaining_budget,
    };
    let mut prompt = format!(
        "{EXECUTION_PLANNER_PROMPT}\n\nGenerate only a restricted plan amendment. The goal is immutable.\n<frozen_amendment_context>{}</frozen_amendment_context>",
        serde_json::to_string(&frozen)?
    );
    if let Some((candidate, report)) = repair {
        prompt.push_str(&format!(
            "\nRepair exactly once without new discovery.\n<original_amendment>{candidate}</original_amendment>\n<compiler_report>{report}</compiler_report>"
        ));
    }
    strict_request::<GeneratedAmendmentCandidate>(
        request.planner_model.clone(),
        "generated_amendment_candidate_v1",
        "Generate one strict restricted execution-plan amendment.",
        prompt,
    )
}

/// One normalized planner/compile repository result that can emit a first-apply metric.
pub trait AppliedPlanningAuditMetric {
    /// Emits the metric only when this result carries first-applied durable evidence.
    fn record_if_applied(&self);
}

impl AppliedPlanningAuditMetric for PlannerCallAuditWriteOutcome {
    fn record_if_applied(&self) {
        let Self::Applied(evidence) = self else {
            return;
        };
        record_execution_planner_call(evidence.call, evidence.outcome);
    }
}

impl AppliedPlanningAuditMetric for CompileAuditWriteOutcome {
    fn record_if_applied(&self) {
        let Self::Applied(evidence) = self else {
            return;
        };
        record_execution_compile_duration(
            evidence.source,
            evidence.outcome,
            Duration::from_micros(evidence.duration_micros),
        );
    }
}

/// Emits a planner or compiler metric only for first-applied normalized audit evidence.
pub fn record_applied_planning_audit(result: &impl AppliedPlanningAuditMetric) {
    result.record_if_applied();
}

fn strict_request<T: schemars::JsonSchema>(
    model: ModelId,
    schema_name: &str,
    description: &str,
    prompt: String,
) -> Result<CompletionRequest, serde_json::Error> {
    let schema = serde_json::to_value(schema_for!(T))?;
    Ok(CompletionRequest {
        model: Some(model),
        messages: vec![ContextMessage::system(prompt)],
        tools: Vec::new(),
        max_output_tokens: Some(EXECUTION_PLANNER_MAX_OUTPUT_TOKENS),
        temperature: None,
        response_format: Some(JsonResponseFormat::strict_json_schema(
            schema_name,
            description,
            schema,
        )),
        native_web_search: NativeWebSearchPolicy::Disabled,
        metadata: std::collections::HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use metrics::{
        Counter, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString,
        Unit,
    };
    use moa_core::types::execution_planning::{
        ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlannerCallKind,
        ExecutionPlannerOutcome,
    };
    use moa_execution::repository::{CompileAuditEvidence, PlannerCallAuditEvidence};
    use uuid::Uuid;

    use super::*;

    struct SampleCounter(Arc<AtomicU64>);

    impl HistogramFn for SampleCounter {
        fn record(&self, _value: f64) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct PlanningRecorder {
        planner_calls: Arc<AtomicU64>,
        compiler_calls: Arc<AtomicU64>,
    }

    impl Recorder for PlanningRecorder {
        fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        }

        fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

        fn describe_histogram(
            &self,
            _key: KeyName,
            _unit: Option<Unit>,
            _description: SharedString,
        ) {
        }

        fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
            if key.name() == "moa_execution_planner_calls_total" {
                Counter::from_arc(Arc::clone(&self.planner_calls))
            } else {
                Counter::noop()
            }
        }

        fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }

        fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            if key.name() == "moa_execution_compile_duration_seconds" {
                Histogram::from_arc(Arc::new(SampleCounter(Arc::clone(&self.compiler_calls))))
            } else {
                Histogram::noop()
            }
        }
    }

    #[test]
    fn execution_planning_strict_schema_is_recursive_and_request_disables_tools() {
        // Pins: planner response shape is generated from the production artifact DTO.
        let schema = serde_json::to_value(schema_for!(GeneratedExecutionCandidate))
            .expect("serialize schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema.to_string().contains("wait_signal"));
        assert!(schema.to_string().contains("output"));
    }

    #[test]
    fn durable_planning_metrics_suppress_exact_replay() {
        // Pins: mutation-checking either durable gate to emit on Replayed would make the
        // corresponding count two instead of one.
        let planner = PlannerCallAuditEvidence {
            audit_uid: Uuid::now_v7(),
            call: ExecutionPlannerCallKind::InitialPlan,
            outcome: ExecutionPlannerOutcome::ProviderError,
            duration_micros: 7,
            candidate_hash: None,
        };
        let compile = CompileAuditEvidence {
            audit_uid: Uuid::now_v7(),
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Rejected,
            duration_micros: 11,
            candidate_hash: "a".repeat(64),
            final_plan_hash: None,
        };
        let planner_calls = Arc::new(AtomicU64::new(0));
        let compiler_calls = Arc::new(AtomicU64::new(0));
        let recorder = PlanningRecorder {
            planner_calls: Arc::clone(&planner_calls),
            compiler_calls: Arc::clone(&compiler_calls),
        };
        metrics::with_local_recorder(&recorder, || {
            record_applied_planning_audit(&PlannerCallAuditWriteOutcome::Applied(planner.clone()));
            record_applied_planning_audit(&PlannerCallAuditWriteOutcome::Replayed(planner));
            record_applied_planning_audit(&CompileAuditWriteOutcome::Applied(compile.clone()));
            record_applied_planning_audit(&CompileAuditWriteOutcome::Replayed(compile));
        });
        assert_eq!(planner_calls.load(Ordering::Relaxed), 1);
        assert_eq!(compiler_calls.load(Ordering::Relaxed), 1);
    }
}
