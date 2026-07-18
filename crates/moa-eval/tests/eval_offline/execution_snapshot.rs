//! Offline tests for the execution-eval redacted snapshot contract.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionBudgetLimit, ExecutionCitation, ExecutionGoalContract, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionUsage, RetryPolicy,
};
use moa_core::types::{
    execution_planning::{
        ExecutionPlannerCallKind, ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope,
        ExecutionPlanningAuditPayload, ExecutionSourceProvenance,
    },
    identifiers::{SessionId, TenantId, UserId},
};
use moa_eval::execution::{
    ExecutionEvalSnapshot, ExecutionHarnessEvidence, ExecutionSessionEventSummary,
    ExecutionTaskKindSummary, ExecutionTaskResultClass,
};
use moa_execution::{
    ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate, ExecutionHash,
    ExecutionValidationReport,
    budget::BudgetLedger,
    compiler::CanonicalExecutionPlan,
    repository::{ExecutionRunRecord, ExecutionSchedulingSnapshot, ExecutionTaskRecord},
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionRouteFields, ExecutionRunStatus,
        ExecutionSourceKind, ExecutionTaskId, ExecutionTaskProjection, ExecutionTaskStatus,
        ExecutionTerminalCause, ExecutionTerminalEvidence, ExecutionTerminalReason,
        LogicalTaskKind,
    },
};
use serde_json::{Value, json};
use uuid::Uuid;

const RAW_TASK_SECRET: &str = "raw-task-output-must-not-survive";
const RAW_AUDIT_SECRET: &str = "raw-planner-candidate-must-not-survive";

#[test]
fn execution_snapshot_redacts_task_payloads_and_normalizes_audits_offline() {
    // Pins: eval snapshots retain typed execution state without task outputs, inputs, or raw planner documents.
    let (runtime, records, audits, harness) = runtime_parts(
        ExecutionRunStatus::Completed,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );

    let snapshot = ExecutionEvalSnapshot::from_parts(runtime, records, audits, harness)
        .expect("matching runtime state should produce a redacted eval snapshot");

    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].kind, ExecutionTaskKindSummary::Capability);
    assert_eq!(
        snapshot.tasks[0].result_class,
        Some(ExecutionTaskResultClass::Completed)
    );
    assert_eq!(snapshot.tasks[0].citation_count, 1);
    assert!(snapshot.run.terminal_output_hash.is_some());
    let encoded = serde_json::to_string(&snapshot).expect("snapshot should serialize");
    assert!(!encoded.contains(RAW_TASK_SECRET));
    assert!(!encoded.contains(RAW_AUDIT_SECRET));
    let task = serde_json::to_value(&snapshot.tasks[0]).expect("redacted task should serialize");
    for forbidden in [
        "input",
        "output",
        "error",
        "current_outcome",
        "outcome_audit",
        "generation_history",
    ] {
        assert!(
            task.get(forbidden).is_none(),
            "redacted task unexpectedly contains `{forbidden}`"
        );
    }
}

#[test]
fn execution_snapshot_rejects_projection_task_disagreement_offline() {
    // Pins: eval cannot silently reconcile a stale or incomplete task query with the scheduler projection.
    let (runtime, mut records, audits, harness) = runtime_parts(
        ExecutionRunStatus::Completed,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    records[0].generation = 2;

    let error = ExecutionEvalSnapshot::from_parts(runtime, records, audits, harness)
        .expect_err("generation disagreement must be rejected");

    assert!(
        error
            .to_string()
            .contains("disagrees with the scheduling projection"),
        "unexpected mismatch error: {error}"
    );
}

#[test]
fn execution_snapshot_rejects_missing_task_rows_and_unbounded_evidence_offline() {
    // Pins: the snapshot requires complete task rows and bounded fixture-only evidence.
    let (runtime, _records, audits, harness) = runtime_parts(
        ExecutionRunStatus::Completed,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let error = ExecutionEvalSnapshot::from_parts(runtime, Vec::new(), audits, harness)
        .expect_err("missing complete task rows must be rejected");
    assert!(error.to_string().contains("complete task rows contain 0"));

    let (runtime, records, audits, mut harness) = runtime_parts(
        ExecutionRunStatus::Completed,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    harness.final_response = Some("x".repeat(1_048_577));
    let error = ExecutionEvalSnapshot::from_parts(runtime, records, audits, harness)
        .expect_err("unbounded final response evidence must be rejected");
    assert!(error.to_string().contains("final response exceeds"));
}

pub(super) fn eval_snapshot(
    run_status: ExecutionRunStatus,
    task_statuses: &[(&str, ExecutionTaskStatus)],
) -> ExecutionEvalSnapshot {
    let (runtime, records, audits, harness) = runtime_parts(run_status, task_statuses);
    ExecutionEvalSnapshot::from_parts(runtime, records, audits, harness)
        .expect("test runtime state should produce a valid eval snapshot")
}

pub(super) fn capability_ref() -> CapabilityReference {
    CapabilityReference {
        name: "knowledge.query".to_string(),
        version: "1".to_string(),
    }
}

fn runtime_parts(
    run_status: ExecutionRunStatus,
    task_statuses: &[(&str, ExecutionTaskStatus)],
) -> (
    ExecutionSchedulingSnapshot,
    Vec<ExecutionTaskRecord>,
    Vec<ExecutionPlanningAuditEnvelope>,
    ExecutionHarnessEvidence,
) {
    let run_uid = Uuid::from_u128(0x18f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c111);
    let tenant_id = TenantId::from(Uuid::from_u128(0x28f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c222));
    let session_id = SessionId(Uuid::from_u128(0x38f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c333));
    let records = task_statuses
        .iter()
        .map(|(key, status)| task_record(run_uid, key, *status))
        .collect::<Vec<_>>();
    let projection = ExecutionProjection {
        plan_revision: 1,
        node_statuses: BTreeMap::from([("research".to_string(), node_status(task_statuses))]),
        tasks: records
            .iter()
            .map(|record| ExecutionTaskProjection {
                task_id: record.task_id,
                node_id: record.node_id.clone(),
                item_key: record.item_key.clone(),
                status: record.status,
                attempt: record.attempt,
                generation: record.generation,
                input: record.input.clone(),
                outcome: record.current_outcome.clone(),
            })
            .collect(),
    };
    let completed_tasks = count_status(task_statuses, ExecutionTaskStatus::Completed);
    let failed_tasks = count_status(task_statuses, ExecutionTaskStatus::Failed);
    let cancelled_tasks = count_status(task_statuses, ExecutionTaskStatus::Cancelled);
    let terminal_tasks = completed_tasks
        .saturating_add(failed_tasks)
        .saturating_add(cancelled_tasks);
    let estimate = ExecutionEstimate {
        cost_microusd: terminal_tasks,
        tokens: terminal_tasks,
        tasks: terminal_tasks,
        tool_calls: terminal_tasks,
        retrieved_bytes: terminal_tasks,
    };
    let approved_budget = budget();
    let catalog = ExecutionCapabilityCatalog::build(Vec::new()).expect("empty catalog is valid");
    let plan = canonical_plan(catalog.catalog_hash);
    let now = fixed_time();
    let terminal = terminal_fields(run_status, task_statuses.len());
    let run = ExecutionRunRecord {
        run_uid,
        tenant_id,
        contact_id: None,
        session_id,
        originating_user_sequence_num: 1,
        planning_context_uid: Uuid::from_u128(0x48f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c444),
        planning_context_hash: ExecutionHash::from_bytes([4; 32]),
        owner_user_id: UserId::new("execution-eval-user"),
        goal: goal(),
        initial_plan: plan.clone(),
        active_plan: plan.clone(),
        initial_plan_hash: plan.plan_hash,
        active_plan_hash: plan.plan_hash,
        confirmed_plan_hash: Some(plan.plan_hash),
        plan_revision: 1,
        plan_history: Vec::new(),
        catalog: catalog.clone(),
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: vec![capability_ref()],
            skill_refs: Vec::new(),
        },
        pinned_instruction_skills: Vec::new(),
        source_provenance: ExecutionSourceProvenance::SkillTemplate {
            route_rationale: "The caller selected a pinned execution template.".to_string(),
            skill_template_ref: "skill://execution-eval".to_string(),
            skill_template_revision_uid: Uuid::from_u128(0x58f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c555),
        },
        source_kind: ExecutionSourceKind::SkillTemplate,
        route: ExecutionRouteFields {
            rationale: "The caller selected a pinned execution template.".to_string(),
        },
        skill_template_ref: Some("skill://execution-eval".to_string()),
        skill_template_revision_uid: Some(Uuid::from_u128(
            0x58f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c555,
        )),
        input: json!({ "query": "screen issuers" }),
        output: terminal.0,
        completion_check_results: vec![json!({
            "check_id": "coverage-check",
            "passed": run_status == ExecutionRunStatus::Completed,
            "evidence": { "expected": task_statuses.len() }
        })],
        terminal_gaps: terminal.1,
        terminal_evidence: terminal.2,
        terminal_reason: terminal.3,
        status: run_status,
        approved_budget: approved_budget.clone(),
        reserved: ExecutionEstimate::default(),
        consumed: estimate,
        budget_overrun: false,
        progress_total_tasks: usize_to_u64(task_statuses.len()),
        progress_completed_tasks: completed_tasks,
        progress_failed_tasks: failed_tasks,
        progress_cancelled_tasks: cancelled_tasks,
        waiting_reasons: Vec::new(),
        wake_epoch: 1,
        processed_wake_epoch: 1,
        idempotency_key: Some("execution-eval-fixture".to_string()),
        cancellation_reason: None,
        created_at: now,
        queued_at: Some(now),
        updated_at: now,
        started_at: Some(now),
        completed_at: run_status.is_terminal().then_some(now),
        confirmed_at: Some(now),
    };
    let runtime = ExecutionSchedulingSnapshot {
        run,
        catalog,
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: vec![capability_ref()],
            skill_refs: Vec::new(),
        },
        pinned_instruction_skills: Vec::new(),
        budget_ledger: BudgetLedger {
            limit: approved_budget,
            reserved: ExecutionEstimate::default(),
            consumed: estimate,
            overrun: false,
        },
        projection,
    };
    let audits = vec![ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id: Some(session_id),
        originating_sequence: Some(1),
        payload: ExecutionPlanningAuditPayload::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionPlannerOutcome::Accepted,
            provider_model: "scripted-planner".to_string(),
            prompt_version: "execution-planner".to_string(),
            candidate_hash: Some("a".repeat(64)),
            candidate_json: Some(RAW_AUDIT_SECRET.to_string()),
            compiler_report: Some("raw compiler report".to_string()),
            duration_micros: 10,
            created_at: now,
        },
    }];
    let harness = ExecutionHarnessEvidence {
        session_events: ExecutionSessionEventSummary {
            run_started: 1,
            progress: usize_to_u64(task_statuses.len()),
            input_required: 0,
            terminal: u64::from(run_status.is_terminal()),
            error: 0,
            raw_task_output: 0,
        },
        capability_calls: Vec::new(),
        final_response: Some("Covered issuers with explicit gaps.".to_string()),
    };
    (runtime, records, audits, harness)
}

fn task_record(run_uid: Uuid, item_key: &str, status: ExecutionTaskStatus) -> ExecutionTaskRecord {
    let terminal = status.is_terminal();
    let usage = ExecutionUsage {
        cost_microusd: u64::from(terminal),
        tokens: u64::from(terminal),
        tool_calls: u64::from(terminal),
        retrieved_bytes: u64::from(terminal),
    };
    let citations = if status == ExecutionTaskStatus::Completed {
        vec![ExecutionCitation {
            source_id: format!("source-{item_key}"),
            uri: None,
            locator: None,
        }]
    } else {
        Vec::new()
    };
    let outcome = match status {
        ExecutionTaskStatus::Completed => Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage.clone(),
            result: ExecutionTaskResult::Completed {
                output: json!({ "secret": RAW_TASK_SECRET }),
                citations: citations.clone(),
            },
        }),
        ExecutionTaskStatus::Failed => Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage.clone(),
            result: ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
                message: "fixture failure".to_string(),
            },
        }),
        ExecutionTaskStatus::Cancelled => Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage.clone(),
            result: ExecutionTaskResult::Cancelled {
                reason: "fixture cancellation".to_string(),
            },
        }),
        _ => None,
    };
    let now = fixed_time();
    ExecutionTaskRecord {
        task_id: ExecutionTaskId::derive(run_uid, "research", item_key)
            .expect("fixture task ID should derive"),
        run_uid,
        tenant_id: TenantId::from(Uuid::from_u128(0x28f8_f1f3_6a67_c90a_7f8f_2f2f_57f5_c222)),
        contact_id: None,
        node_id: "research".to_string(),
        item_key: item_key.to_string(),
        requirement_ids: vec!["screen-all".to_string()],
        plan_revision: 1,
        status,
        attempt: 1,
        generation: 1,
        input: json!({ "issuer": item_key, "secret": RAW_TASK_SECRET }),
        resume_input_history: Vec::new(),
        kind: LogicalTaskKind::Capability {
            reference: capability_ref(),
        },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        estimate: ExecutionEstimate {
            cost_microusd: 1,
            tokens: 1,
            tasks: 1,
            tool_calls: 1,
            retrieved_bytes: 1,
        },
        reserved: ExecutionEstimate::default(),
        actual: usage,
        actual_tasks: u64::from(terminal),
        current_outcome: outcome,
        output: (status == ExecutionTaskStatus::Completed)
            .then(|| json!({ "secret": RAW_TASK_SECRET })),
        error: None,
        citations,
        generation_history: vec![json!({ "secret": RAW_TASK_SECRET })],
        outcome_audit: vec![json!({ "output": RAW_TASK_SECRET })],
        created_at: now,
        updated_at: now,
        reserved_at: terminal.then_some(now),
        started_at: terminal.then_some(now),
        completed_at: terminal.then_some(now),
    }
}

fn goal() -> ExecutionGoalContract {
    ExecutionGoalContract {
        objective: "Screen every issuer".to_string(),
        requirements: vec![ExecutionRequirement {
            id: "screen-all".to_string(),
            description: "Screen every expected issuer".to_string(),
        }],
        deliverables: Vec::new(),
        coverage: vec![CoverageRequirement {
            id: "issuer-coverage".to_string(),
            description: "Cover the independent issuer universe".to_string(),
            map_node_id: "research".to_string(),
            expected_items: json!(["issuer-a", "issuer-b"]),
            require_all: true,
        }],
        constraints: Vec::new(),
        completion_checks: vec![CompletionCheck {
            id: "coverage-check".to_string(),
            description: "All issuers are covered".to_string(),
            requirement_ids: vec!["screen-all".to_string()],
            constraint_ids: Vec::new(),
            kind: CompletionCheckKind::MapCoverage {
                map_node_id: "research".to_string(),
            },
        }],
    }
}

fn canonical_plan(catalog_hash: ExecutionHash) -> CanonicalExecutionPlan {
    CanonicalExecutionPlan {
        definition: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes: vec![moa_artifacts::execution_plan::ExecutionNode {
                id: "research".to_string(),
                requirement_ids: vec!["screen-all".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({ "type": "object" }),
                operation: ExecutionOperation::Map {
                    items: json!({ "$ref": "$.input.issuers" }),
                    item_key: "/issuer".to_string(),
                    max_items: 10,
                    item_output_schema: json!({ "type": "object" }),
                    task: moa_artifacts::execution_plan::MapTask::Capability {
                        reference: capability_ref(),
                    },
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    max_backoff_ms: 1,
                },
                budget: None,
            }],
        },
        plan_hash: ExecutionHash::from_bytes([7; 32]),
        catalog_hash,
        estimate: ExecutionEstimate {
            cost_microusd: 10,
            tokens: 10,
            tasks: 10,
            tool_calls: 10,
            retrieved_bytes: 10,
        },
        report: ExecutionValidationReport::default(),
    }
}

fn budget() -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(10),
        max_tokens: Some(10),
        max_tasks: Some(10),
        max_tool_calls: Some(10),
        max_retrieved_bytes: Some(10),
        deadline_at: None,
    }
}

fn terminal_fields(
    status: ExecutionRunStatus,
    task_count: usize,
) -> (
    Option<Value>,
    Vec<String>,
    Option<ExecutionTerminalEvidence>,
    Option<ExecutionTerminalReason>,
) {
    match status {
        ExecutionRunStatus::Completed => (
            Some(json!({ "covered": task_count })),
            Vec::new(),
            Some(ExecutionTerminalEvidence {
                cause: ExecutionTerminalCause::Completion { limit_stop: None },
                satisfied_requirement_count: 1,
                requirement_count: 1,
            }),
            Some(ExecutionTerminalReason::Completed),
        ),
        ExecutionRunStatus::Partial => (
            Some(json!({ "covered": task_count })),
            vec!["issuer coverage is incomplete".to_string()],
            Some(ExecutionTerminalEvidence {
                cause: ExecutionTerminalCause::Completion { limit_stop: None },
                satisfied_requirement_count: 0,
                requirement_count: 1,
            }),
            Some(ExecutionTerminalReason::GoalIncomplete),
        ),
        _ => (None, Vec::new(), None, None),
    }
}

fn node_status(task_statuses: &[(&str, ExecutionTaskStatus)]) -> ExecutionNodeStatus {
    if task_statuses
        .iter()
        .all(|(_, status)| *status == ExecutionTaskStatus::Completed)
    {
        ExecutionNodeStatus::Completed
    } else if task_statuses
        .iter()
        .any(|(_, status)| *status == ExecutionTaskStatus::Failed)
    {
        ExecutionNodeStatus::Failed
    } else {
        ExecutionNodeStatus::Running
    }
}

fn count_status(
    task_statuses: &[(&str, ExecutionTaskStatus)],
    expected: ExecutionTaskStatus,
) -> u64 {
    usize_to_u64(
        task_statuses
            .iter()
            .filter(|(_, status)| *status == expected)
            .count(),
    )
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).expect("small test fixture count should fit u64")
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("fixture timestamp should be valid")
}
