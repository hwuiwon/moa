//! Redacted execution-run state consumed by deterministic evaluation predicates.

use std::collections::BTreeSet;

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionGoalContract, ExecutionTaskResult,
    ExecutionUsage,
};
use moa_core::types::execution_planning::{
    ExecutionCompileOutcome, ExecutionCompileSource, ExecutionMode, ExecutionPlannerCallKind,
    ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelopeV1, ExecutionPlanningAuditPayloadV1,
    ExecutionRouteDecisionKind, ExecutionRouteReason, ExecutionRouteStage,
    ExecutionSourceProvenanceV1,
};
use moa_eval_core::{EvalError, Result};
use moa_execution::{
    ExecutionEstimate,
    budget::BudgetLedger,
    completion::CompletionCheckResult,
    repository::{ExecutionSchedulingSnapshot, ExecutionTaskRecord},
    state::{
        ExecutionRouteFields, ExecutionRunStatus, ExecutionSourceKind, ExecutionTaskStatus,
        ExecutionTerminalEvidence, ExecutionTerminalReason, LogicalTaskKind,
    },
};
use serde::{Deserialize, Serialize};
use serde_canonical_json::CanonicalFormatter;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Current schema version for execution-eval snapshots.
pub const EXECUTION_EVAL_SNAPSHOT_SCHEMA_VERSION: u8 = 1;
const TERMINAL_OUTPUT_HASH_DOMAIN: &[u8] = b"moa.execution-eval.terminal-output.v1\0";
const MAX_CAPABILITY_OBSERVATIONS: usize = 10_000;
const MAX_FINAL_RESPONSE_BYTES: usize = 1_048_576;
const MAX_OBSERVATION_TEXT_BYTES: usize = 512;

/// Redacted state for one execution run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalRunV1 {
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Immutable user-derived goal contract.
    pub goal: ExecutionGoalContract,
    /// Current canonical plan hash.
    pub active_plan_hash: String,
    /// Current one-based plan revision.
    pub plan_revision: u64,
    /// Normalized source cohort.
    pub source_kind: ExecutionSourceKind,
    /// Exact admitted source provenance.
    pub source_provenance: ExecutionSourceProvenanceV1,
    /// Normalized run-producing route.
    pub route: ExecutionRouteFields,
    /// Current durable run status.
    pub status: ExecutionRunStatus,
    /// Terminal structured output retained only for in-process scoring.
    pub terminal_output: Option<Value>,
    /// Domain-separated canonical hash of the terminal output.
    pub terminal_output_hash: Option<String>,
    /// Persisted typed completion-check results.
    pub completion_check_results: Vec<CompletionCheckResult>,
    /// Explicit terminal completion gaps.
    pub terminal_gaps: Vec<String>,
    /// Typed terminal cause and requirement counts.
    pub terminal_evidence: Option<ExecutionTerminalEvidence>,
    /// Normalized terminal reason.
    pub terminal_reason: Option<ExecutionTerminalReason>,
    /// Approved, reserved, and consumed resource accounting.
    pub budget_ledger: BudgetLedger,
    /// Persisted task progress counters.
    pub progress: ExecutionProgressSummaryV1,
}

/// Persisted run progress counters used for projection consistency checks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProgressSummaryV1 {
    /// Number of materialized tasks.
    pub total_tasks: u64,
    /// Number of completed tasks.
    pub completed_tasks: u64,
    /// Number of failed tasks.
    pub failed_tasks: u64,
    /// Number of cancelled tasks.
    pub cancelled_tasks: u64,
}

/// Redacted persisted state for one logical execution task.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalTaskV1 {
    /// Stable logical task identifier.
    pub task_id: moa_execution::state::ExecutionTaskId,
    /// Stable plan node identifier.
    pub node_id: String,
    /// Stable ordinary, map, reducer, or verifier item key.
    pub item_key: String,
    /// Current durable task status.
    pub status: ExecutionTaskStatus,
    /// One-based execution attempt.
    pub attempt: u32,
    /// One-based dispatch generation fence.
    pub generation: u64,
    /// Redacted task-kind category.
    pub kind: ExecutionTaskKindSummaryV1,
    /// Governed capabilities available to or invoked by the task.
    pub capability_refs: Vec<CapabilityReference>,
    /// Latest typed result category without result payloads.
    pub result_class: Option<ExecutionTaskResultClassV1>,
    /// Cumulative resource usage.
    pub usage: ExecutionUsage,
    /// Whether this logical task was reconciled terminally.
    pub actual_tasks: u64,
    /// Number of accepted provenance citations.
    pub citation_count: u64,
}

/// Redacted task-kind category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTaskKindSummaryV1 {
    /// Direct governed capability invocation.
    Capability,
    /// Bounded task-local agent.
    Agent,
    /// Tenant review wait.
    Review,
    /// Named external signal wait.
    WaitSignal,
    /// Terminal output task.
    Output,
    /// Bounded semantic completion verifier.
    CompletionVerifier,
}

/// Redacted task-result category.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionTaskResultClassV1 {
    /// Task completed successfully.
    Completed,
    /// Task requires declared-audience input.
    NeedsInput,
    /// Task requires a compiler-validated plan amendment.
    NeedsReplan,
    /// Task was cancelled.
    Cancelled,
    /// Task failed with the retained typed failure class.
    Failed {
        /// Stable execution failure class.
        class: ExecutionFailureClass,
    },
}

/// Normalized planning-audit fields retained for deterministic evaluation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionPlanningAuditSummaryV1 {
    /// One deterministic routing decision.
    Route {
        /// Initial or Act-escalation stage.
        stage: ExecutionRouteStage,
        /// Needs-input or routed decision.
        decision: ExecutionRouteDecisionKind,
        /// Selected execution mode, when routed.
        mode: Option<ExecutionMode>,
        /// Stable route reason.
        reason: ExecutionRouteReason,
    },
    /// One actual provider planner call.
    PlannerCall {
        /// Initial, repair, amendment, or amendment-repair call.
        call_kind: ExecutionPlannerCallKind,
        /// Zero for the first call and one for its repair.
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
        /// Candidate hash when present.
        candidate_hash: Option<String>,
    },
    /// One pure compiler call.
    Compile {
        /// Compiler input cohort.
        source: ExecutionCompileSource,
        /// Stable source-specific operation key.
        operation_key: String,
        /// Run identifier for amendments.
        run_uid: Option<Uuid>,
        /// Plan revision for amendments.
        plan_revision: Option<u64>,
        /// Closed compiler outcome.
        outcome: ExecutionCompileOutcome,
        /// Hash of the strict compile candidate.
        candidate_hash: String,
        /// Accepted final plan hash, when compilation succeeded.
        final_plan_hash: Option<String>,
    },
}

/// Bounded counts of execution-related session event categories.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSessionEventSummaryV1 {
    /// Number of run-start events.
    pub run_started: u64,
    /// Number of progress events.
    pub progress: u64,
    /// Number of input-required events.
    pub input_required: u64,
    /// Number of terminal events.
    pub terminal: u64,
    /// Number of error events.
    pub error: u64,
    /// Number of events that exposed a raw task output.
    pub raw_task_output: u64,
}

impl ExecutionSessionEventSummaryV1 {
    /// Looks up one stable event-category count.
    #[must_use]
    pub fn count(&self, event_kind: &str) -> Option<u64> {
        match event_kind {
            "run_started" => Some(self.run_started),
            "progress" => Some(self.progress),
            "input_required" => Some(self.input_required),
            "terminal" => Some(self.terminal),
            "error" => Some(self.error),
            "raw_task_output" => Some(self.raw_task_output),
            _ => None,
        }
    }
}

/// One fixture-observed logical capability invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCapabilityCallObservationV1 {
    /// Fixture-stable logical invocation identifier.
    pub logical_invocation_id: String,
    /// Exact governed capability reference.
    pub reference: CapabilityReference,
    /// Stable map item key, when applicable.
    pub item_key: Option<String>,
    /// Whether this observation was a replay rather than a new logical effect.
    pub replayed: bool,
}

/// Optional deterministic fixture evidence outside run/task persistence.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHarnessEvidenceV1 {
    /// Bounded session-event category counts.
    pub session_events: ExecutionSessionEventSummaryV1,
    /// Optional logical capability-call observations.
    pub capability_calls: Vec<ExecutionCapabilityCallObservationV1>,
    /// Final synthesized response retained only for in-process scoring.
    pub final_response: Option<String>,
}

/// Strict redacted read model assembled from the same state consumed by the runtime.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalSnapshotV1 {
    /// Snapshot schema version, fixed at `1`.
    pub schema_version: u8,
    /// Redacted durable run state.
    pub run: ExecutionEvalRunV1,
    /// Complete ordered redacted task state.
    pub tasks: Vec<ExecutionEvalTaskV1>,
    /// Normalized route, planner, and compiler audits.
    pub planning_audits: Vec<ExecutionPlanningAuditSummaryV1>,
    /// Optional deterministic fixture evidence.
    pub harness: ExecutionHarnessEvidenceV1,
}

impl ExecutionEvalSnapshotV1 {
    /// Builds a strict redacted snapshot from runtime-owned typed state.
    pub fn from_parts(
        snapshot: ExecutionSchedulingSnapshot,
        task_records: Vec<ExecutionTaskRecord>,
        planning_audits: Vec<ExecutionPlanningAuditEnvelopeV1>,
        harness: ExecutionHarnessEvidenceV1,
    ) -> Result<Self> {
        validate_task_projection(&snapshot, &task_records)?;
        validate_harness(&harness)?;

        let completion_check_results = snapshot
            .run
            .completion_check_results
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<CompletionCheckResult>, _>>()
            .map_err(|error| {
                invalid_config(format!(
                    "execution completion-check evidence is not typed V1 data: {error}"
                ))
            })?;
        ensure_unique_check_ids(&completion_check_results)?;

        let terminal_output_hash = snapshot
            .run
            .output
            .as_ref()
            .map(canonical_terminal_output_hash)
            .transpose()?;
        let run = ExecutionEvalRunV1 {
            run_uid: snapshot.run.run_uid,
            goal: snapshot.run.goal.clone(),
            active_plan_hash: snapshot.run.active_plan_hash.to_string(),
            plan_revision: snapshot.run.plan_revision,
            source_kind: snapshot.run.source_kind,
            source_provenance: snapshot.run.source_provenance.clone(),
            route: snapshot.run.route,
            status: snapshot.run.status,
            terminal_output: snapshot.run.output.clone(),
            terminal_output_hash,
            completion_check_results,
            terminal_gaps: snapshot.run.terminal_gaps.clone(),
            terminal_evidence: snapshot.run.terminal_evidence.clone(),
            terminal_reason: snapshot.run.terminal_reason,
            budget_ledger: snapshot.budget_ledger,
            progress: ExecutionProgressSummaryV1 {
                total_tasks: snapshot.run.progress_total_tasks,
                completed_tasks: snapshot.run.progress_completed_tasks,
                failed_tasks: snapshot.run.progress_failed_tasks,
                cancelled_tasks: snapshot.run.progress_cancelled_tasks,
            },
        };
        let tasks = task_records
            .iter()
            .map(redact_task)
            .collect::<Result<Vec<_>>>()?;
        let planning_audits = planning_audits
            .iter()
            .map(normalize_audit)
            .collect::<Vec<_>>();

        Ok(Self {
            schema_version: EXECUTION_EVAL_SNAPSHOT_SCHEMA_VERSION,
            run,
            tasks,
            planning_audits,
            harness,
        })
    }
}

fn validate_task_projection(
    snapshot: &ExecutionSchedulingSnapshot,
    records: &[ExecutionTaskRecord],
) -> Result<()> {
    if snapshot.projection.tasks.len() != records.len() {
        return Err(invalid_config(format!(
            "execution projection contains {} tasks but the complete task rows contain {}",
            snapshot.projection.tasks.len(),
            records.len()
        )));
    }
    for (index, (projection, record)) in snapshot.projection.tasks.iter().zip(records).enumerate() {
        if record.run_uid != snapshot.run.run_uid {
            return Err(invalid_config(format!(
                "execution task row {index} belongs to run {} instead of {}",
                record.run_uid, snapshot.run.run_uid
            )));
        }
        if (
            projection.task_id,
            projection.node_id.as_str(),
            projection.item_key.as_str(),
            projection.generation,
            projection.status,
        ) != (
            record.task_id,
            record.node_id.as_str(),
            record.item_key.as_str(),
            record.generation,
            record.status,
        ) {
            return Err(invalid_config(format!(
                "execution task row {index} disagrees with the scheduling projection"
            )));
        }
    }
    Ok(())
}

fn ensure_unique_check_ids(checks: &[CompletionCheckResult]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for check in checks {
        if check.check_id.trim().is_empty() || !ids.insert(check.check_id.as_str()) {
            return Err(invalid_config(format!(
                "execution completion-check ID `{}` is empty or duplicated",
                check.check_id
            )));
        }
    }
    Ok(())
}

fn validate_harness(harness: &ExecutionHarnessEvidenceV1) -> Result<()> {
    if harness.capability_calls.len() > MAX_CAPABILITY_OBSERVATIONS {
        return Err(invalid_config(format!(
            "execution harness supplied {} capability observations; maximum is {MAX_CAPABILITY_OBSERVATIONS}",
            harness.capability_calls.len()
        )));
    }
    if harness
        .final_response
        .as_ref()
        .is_some_and(|response| response.len() > MAX_FINAL_RESPONSE_BYTES)
    {
        return Err(invalid_config(format!(
            "execution final response exceeds {MAX_FINAL_RESPONSE_BYTES} bytes"
        )));
    }
    for observation in &harness.capability_calls {
        validate_bounded_text(
            "logical capability invocation ID",
            &observation.logical_invocation_id,
        )?;
        validate_bounded_text("capability name", &observation.reference.name)?;
        validate_bounded_text("capability version", &observation.reference.version)?;
        if let Some(item_key) = observation.item_key.as_deref() {
            validate_bounded_text("capability item key", item_key)?;
        }
    }
    Ok(())
}

fn validate_bounded_text(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > MAX_OBSERVATION_TEXT_BYTES {
        return Err(invalid_config(format!(
            "{name} must contain 1..={MAX_OBSERVATION_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

fn redact_task(record: &ExecutionTaskRecord) -> Result<ExecutionEvalTaskV1> {
    let (kind, capability_refs) = match &record.kind {
        LogicalTaskKind::Capability { reference } => (
            ExecutionTaskKindSummaryV1::Capability,
            vec![reference.clone()],
        ),
        LogicalTaskKind::Agent {
            capability_refs, ..
        } => (ExecutionTaskKindSummaryV1::Agent, capability_refs.clone()),
        LogicalTaskKind::Review { .. } => (ExecutionTaskKindSummaryV1::Review, Vec::new()),
        LogicalTaskKind::WaitSignal { .. } => (ExecutionTaskKindSummaryV1::WaitSignal, Vec::new()),
        LogicalTaskKind::Output { .. } => (ExecutionTaskKindSummaryV1::Output, Vec::new()),
        LogicalTaskKind::CompletionVerifier { .. } => {
            (ExecutionTaskKindSummaryV1::CompletionVerifier, Vec::new())
        }
    };
    let citation_count = u64::try_from(record.citations.len())
        .map_err(|_| invalid_config("execution task citation count exceeds u64".to_string()))?;
    Ok(ExecutionEvalTaskV1 {
        task_id: record.task_id,
        node_id: record.node_id.clone(),
        item_key: record.item_key.clone(),
        status: record.status,
        attempt: record.attempt,
        generation: record.generation,
        kind,
        capability_refs,
        result_class: record.current_outcome.as_ref().map(result_class),
        usage: record.actual.clone(),
        actual_tasks: record.actual_tasks,
        citation_count,
    })
}

fn result_class(
    outcome: &moa_artifacts::execution_plan::ExecutionTaskOutcome,
) -> ExecutionTaskResultClassV1 {
    match &outcome.result {
        ExecutionTaskResult::Completed { .. } => ExecutionTaskResultClassV1::Completed,
        ExecutionTaskResult::NeedsInput { .. } => ExecutionTaskResultClassV1::NeedsInput,
        ExecutionTaskResult::NeedsReplan { .. } => ExecutionTaskResultClassV1::NeedsReplan,
        ExecutionTaskResult::Cancelled { .. } => ExecutionTaskResultClassV1::Cancelled,
        ExecutionTaskResult::Failed { class, .. } => ExecutionTaskResultClassV1::Failed {
            class: class.clone(),
        },
    }
}

fn normalize_audit(audit: &ExecutionPlanningAuditEnvelopeV1) -> ExecutionPlanningAuditSummaryV1 {
    match &audit.payload {
        ExecutionPlanningAuditPayloadV1::Route {
            stage,
            decision,
            mode,
            reason,
            ..
        } => ExecutionPlanningAuditSummaryV1::Route {
            stage: *stage,
            decision: *decision,
            mode: *mode,
            reason: *reason,
        },
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind,
            call_ordinal,
            run_uid,
            plan_revision,
            outcome,
            provider_model,
            prompt_version,
            candidate_hash,
            ..
        } => ExecutionPlanningAuditSummaryV1::PlannerCall {
            call_kind: *call_kind,
            call_ordinal: *call_ordinal,
            run_uid: *run_uid,
            plan_revision: *plan_revision,
            outcome: *outcome,
            provider_model: provider_model.clone(),
            prompt_version: prompt_version.clone(),
            candidate_hash: candidate_hash.clone(),
        },
        ExecutionPlanningAuditPayloadV1::Compile {
            source,
            operation_key,
            run_uid,
            plan_revision,
            outcome,
            candidate_hash,
            final_plan_hash,
            ..
        } => ExecutionPlanningAuditSummaryV1::Compile {
            source: *source,
            operation_key: operation_key.clone(),
            run_uid: *run_uid,
            plan_revision: *plan_revision,
            outcome: *outcome,
            candidate_hash: candidate_hash.clone(),
            final_plan_hash: final_plan_hash.clone(),
        },
    }
}

fn canonical_terminal_output_hash(value: &Value) -> Result<String> {
    let mut serializer =
        serde_json::Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
    value.serialize(&mut serializer)?;
    let mut hasher = Sha256::new();
    hasher.update(TERMINAL_OUTPUT_HASH_DOMAIN);
    hasher.update(serializer.into_inner());
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn total_accounted_resources(
    snapshot: &ExecutionEvalSnapshotV1,
) -> Option<ExecutionEstimate> {
    snapshot
        .run
        .budget_ledger
        .reserved
        .checked_add(
            snapshot.run.budget_ledger.consumed,
            "execution eval budget total",
        )
        .ok()
}

fn invalid_config(message: String) -> EvalError {
    EvalError::InvalidConfig(message)
}
