//! Regenerates the checked-in deterministic execution routing and contract corpora.

use std::{fs, path::PathBuf};

use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionConstraint, ExecutionDeliverable, ExecutionGoalContract, ExecutionNode,
    ExecutionOperation, ExecutionPlanDefinition, ExecutionRequirement, GeneratedExecutionCandidate,
    MapTask, RetryPolicy,
};
use moa_brain::execution_planning::{
    ExecutionRouteClassifierLabelV1, ExecutionRouteClassifierOutputV1,
};
use moa_core::types::{
    completion::TokenUsage,
    execution_planning::{
        DurableUpgradeSignal, ExecutionPlanningEvidence, ExecutionRouteClassifierOutcome,
        ExecutionRouteReason, ExecutionStrategy,
    },
};
use moa_eval::execution::{
    CompletionCheckExpectationV1, CoverageExpectationV1, DeliverableExpectationV1,
    ExecutionContractCaseV1, ExecutionContractExpectationsV1, ExecutionRoutingCaseV1,
    ExecutionRoutingClassifierFixtureV1, ExecutionRoutingLabelV1, RunInputExpectationV1,
    TextExpectationV1, contract::CompletionCheckKindExpectationV1,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution");
    fs::create_dir_all(&root)?;
    let routing = jsonl(&routing_cases())?;
    let contract = jsonl(&contract_cases())?;
    let task_quality = fs::read(root.join("task-quality-v1.jsonl"))?;
    fs::write(root.join("routing-v1.jsonl"), &routing)?;
    fs::write(root.join("contract-recorded-v1.jsonl"), &contract)?;
    let manifest = format!(
        "schema_version = 1\n\n[routing]\npath = \"routing-v1.jsonl\"\nsha256 = \"{}\"\ncount = 320\n\n[contract]\npath = \"contract-recorded-v1.jsonl\"\nsha256 = \"{}\"\ncount = 80\n\n[task_quality]\npath = \"task-quality-v1.jsonl\"\nsha256 = \"{}\"\ncount = 20\n",
        sha256(&routing),
        sha256(&contract),
        sha256(&task_quality),
    );
    fs::write(root.join("manifest.toml"), manifest)?;
    Ok(())
}

fn jsonl<T: Serialize>(records: &[T]) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn routing_cases() -> Vec<ExecutionRoutingCaseV1> {
    let mut cases = Vec::with_capacity(320);
    for index in 0..60 {
        cases.push(route_case(
            format!("respond-{index:03}"),
            format!("Explain the stable concept numbered {index} in one concise answer."),
            ExecutionRoutingLabelV1::Respond,
            None,
            ExecutionRouteReason::SimpleResponse,
            response_fixture(
                ExecutionRouteClassifierLabelV1::Respond,
                ExecutionRouteReason::SimpleResponse,
                9_500,
                Vec::new(),
            ),
            ExecutionRouteClassifierOutcome::Accepted,
        ));
    }
    for index in 0..140 {
        let mut case = route_case(
            format!("execute-inline-{index:03}"),
            format!(
                "Investigate the bounded issue numbered {index} and use the available context."
            ),
            ExecutionRoutingLabelV1::Execute,
            Some(ExecutionStrategy::Inline),
            ExecutionRouteReason::BoundedInteractiveWork,
            response_fixture(
                ExecutionRouteClassifierLabelV1::Execute,
                ExecutionRouteReason::BoundedInteractiveWork,
                9_000,
                Vec::new(),
            ),
            ExecutionRouteClassifierOutcome::Accepted,
        );
        case.near_boundary = index < 80;
        if case.near_boundary {
            case.objective =
                format!("Look into why bounded signal {index} changed and report back.");
            case.tags.push("near-boundary".to_string());
        }
        if (80..104).contains(&index) {
            let fault_index = index - 80;
            case.tags.push("classifier-fallback".to_string());
            match fault_index / 4 {
                0 => {
                    case.classifier = ExecutionRoutingClassifierFixtureV1::ProviderError;
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::ProviderError;
                }
                1 => {
                    case.classifier = ExecutionRoutingClassifierFixtureV1::StreamError;
                    case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::StreamError;
                }
                2 => {
                    case.classifier = ExecutionRoutingClassifierFixtureV1::Malformed;
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::SchemaRejected;
                }
                3 => {
                    case.classifier = ExecutionRoutingClassifierFixtureV1::Oversized;
                    case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::Oversized;
                }
                4 => {
                    case.classifier = response_fixture(
                        ExecutionRouteClassifierLabelV1::Execute,
                        ExecutionRouteReason::BulkCollection,
                        7_999,
                        Vec::new(),
                    );
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::LowConfidence;
                }
                _ => {
                    case.classifier = response_fixture(
                        ExecutionRouteClassifierLabelV1::Respond,
                        ExecutionRouteReason::BulkCollection,
                        9_500,
                        Vec::new(),
                    );
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::InvalidDecision;
                }
            }
        } else if (104..112).contains(&index) {
            case.classifier = response_fixture(
                ExecutionRouteClassifierLabelV1::Respond,
                ExecutionRouteReason::SimpleResponse,
                9_500,
                Vec::new(),
            );
            case.attachment_count = 1;
            case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::ContextForcedInline;
            case.tags.push("context-forced-inline".to_string());
        }
        cases.push(case);
    }
    for index in 0..100 {
        let objective = if index == 0 {
            "Screen all of the S&P 500 over five years and count AI mentions across the complete knowledge base.".to_string()
        } else {
            format!(
                "Process the complete bulk universe {index} durably and produce a verified report."
            )
        };
        let mut case = route_case(
            format!("execute-durable-{index:03}"),
            objective.clone(),
            ExecutionRoutingLabelV1::Execute,
            Some(ExecutionStrategy::Durable),
            if index == 0 {
                ExecutionRouteReason::BulkCollection
            } else {
                ExecutionRouteReason::HighFanout
            },
            response_fixture(
                ExecutionRouteClassifierLabelV1::Execute,
                if index == 0 {
                    ExecutionRouteReason::BulkCollection
                } else {
                    ExecutionRouteReason::HighFanout
                },
                9_500,
                Vec::new(),
            ),
            ExecutionRouteClassifierOutcome::Accepted,
        );
        if index == 0 {
            case.tags.push("sp500-ai-five-year-screen".to_string());
        }
        if index >= 60 {
            let evidence = vec![ExecutionPlanningEvidence {
                source: "inline-tool".to_string(),
                summary: format!("discovered bulk universe {index}"),
                value: json!({"items": 500, "case": index}),
            }];
            case.expected_reason = ExecutionRouteReason::DurableUpgrade;
            case.classifier = ExecutionRoutingClassifierFixtureV1::NotCalled;
            case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::NotCalled;
            case.durable_upgrade = Some(DurableUpgradeSignal {
                objective,
                reason: ExecutionRouteReason::HighFanout,
                evidence: evidence.clone(),
            });
            case.expected_durable_upgrade_evidence = Some(evidence);
            case.tags.push("durable-upgrade".to_string());
        }
        cases.push(case);
    }
    for index in 0..20 {
        let mut case = route_case(
            format!("needs-input-{index:03}"),
            format!("Screen the requested dataset {index}."),
            ExecutionRoutingLabelV1::NeedsInput,
            None,
            ExecutionRouteReason::PreflightInputMissing,
            response_fixture(
                ExecutionRouteClassifierLabelV1::NeedsInput,
                ExecutionRouteReason::PreflightInputMissing,
                9_500,
                vec!["coverage universe".to_string()],
            ),
            ExecutionRouteClassifierOutcome::Accepted,
        );
        case.tags.push("clarification".to_string());
        cases.push(case);
    }
    cases
}

fn route_case(
    case_id: String,
    objective: String,
    expected_label: ExecutionRoutingLabelV1,
    expected_strategy: Option<ExecutionStrategy>,
    expected_reason: ExecutionRouteReason,
    classifier: ExecutionRoutingClassifierFixtureV1,
    expected_classifier_outcome: ExecutionRouteClassifierOutcome,
) -> ExecutionRoutingCaseV1 {
    ExecutionRoutingCaseV1 {
        schema_version: 1,
        case_id,
        objective,
        attachment_count: 0,
        has_recent_target: false,
        classifier,
        expected_classifier_outcome,
        expected_label,
        expected_strategy,
        expected_reason,
        near_boundary: false,
        durable_upgrade: None,
        expected_durable_upgrade_evidence: None,
        tags: Vec::new(),
    }
}

fn response_fixture(
    label: ExecutionRouteClassifierLabelV1,
    reason: ExecutionRouteReason,
    confidence_bps: u16,
    missing_inputs: Vec<String>,
) -> ExecutionRoutingClassifierFixtureV1 {
    ExecutionRoutingClassifierFixtureV1::Response {
        output: ExecutionRouteClassifierOutputV1 {
            label,
            reason,
            confidence_bps,
            missing_inputs,
        },
        usage: TokenUsage {
            input_tokens_uncached: 24,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 12,
        },
        cost_microusd: 7,
    }
}

fn contract_cases() -> Vec<ExecutionContractCaseV1> {
    (0..80).map(contract_case).collect()
}

fn contract_case(index: usize) -> ExecutionContractCaseV1 {
    let suffix = format!("{index:03}");
    let keys = vec![
        format!("issuer-{suffix}-a"),
        format!("issuer-{suffix}-b"),
        format!("issuer-{suffix}-c"),
    ];
    let requirement_screen = format!("req-screen-{suffix}");
    let requirement_report = format!("req-report-{suffix}");
    let constraint_exclusions = format!("constraint-exclusions-{suffix}");
    let constraint_definition = format!("constraint-definition-{suffix}");
    let report_schema = json!({"type": "string", "minLength": 1});
    let item_schema = json!({
        "type": "object",
        "properties": {"count": {"type": "integer"}},
        "required": ["count"],
        "additionalProperties": false
    });
    let output_schema = json!({
        "type": "object",
        "properties": {"report": report_schema.clone()},
        "required": ["report"],
        "additionalProperties": false
    });
    let candidate = GeneratedExecutionCandidate {
        goal: ExecutionGoalContract {
            objective: format!(
                "Screen a complete issuer universe for five years and report defined AI mentions with citations ({suffix})."
            ),
            requirements: vec![
                ExecutionRequirement {
                    id: requirement_screen.clone(),
                    description: "Screen every issuer over the trailing five years for AI mentions with evidence citations.".to_string(),
                },
                ExecutionRequirement {
                    id: requirement_report.clone(),
                    description: "Produce one structured report with issuer counts and source evidence.".to_string(),
                },
            ],
            deliverables: vec![ExecutionDeliverable {
                id: format!("deliverable-report-{suffix}"),
                description: "Final structured research report".to_string(),
                output_pointer: "/report".to_string(),
                schema: report_schema.clone(),
            }],
            coverage: vec![CoverageRequirement {
                id: format!("coverage-{suffix}"),
                description: "Complete issuer universe".to_string(),
                map_node_id: "screen".to_string(),
                expected_items: Value::Array(keys.iter().cloned().map(Value::String).collect()),
                require_all: true,
            }],
            constraints: vec![
                ExecutionConstraint {
                    id: constraint_exclusions.clone(),
                    description: "Exclude duplicate filings and preserve analyst-note provenance.".to_string(),
                },
                ExecutionConstraint {
                    id: constraint_definition.clone(),
                    description: "Define AI mentions as case-insensitive artificial intelligence or AI references.".to_string(),
                },
            ],
            completion_checks: vec![
                CompletionCheck {
                    id: format!("check-coverage-{suffix}"),
                    description: "Require complete map coverage".to_string(),
                    requirement_ids: vec![requirement_screen.clone()],
                    constraint_ids: vec![constraint_exclusions.clone()],
                    kind: CompletionCheckKind::MapCoverage {
                        map_node_id: "screen".to_string(),
                    },
                },
                CompletionCheck {
                    id: format!("check-citations-{suffix}"),
                    description: "Require citations for every issuer".to_string(),
                    requirement_ids: vec![requirement_report.clone()],
                    constraint_ids: vec![constraint_definition.clone()],
                    kind: CompletionCheckKind::Citations {
                        node_ids: vec!["screen".to_string()],
                        min_per_task: 1,
                    },
                },
            ],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "years": {"type": "integer"},
                    "definition": {"type": "string"}
                },
                "required": ["years", "definition"],
                "additionalProperties": false
            }),
            output_schema: output_schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: "screen".to_string(),
                    requirement_ids: vec![requirement_screen.clone()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: json!({"type": "array", "items": item_schema.clone()}),
                    operation: ExecutionOperation::Map {
                        items: Value::Array(
                            keys.iter()
                                .map(|key| json!({"ticker": key}))
                                .collect(),
                        ),
                        item_key: "/ticker".to_string(),
                        max_items: 3,
                        item_output_schema: item_schema,
                        task: MapTask::Capability {
                            reference: CapabilityReference {
                                name: "fixture.research".to_string(),
                                version: "1".to_string(),
                            },
                        },
                    },
                    retry: RetryPolicy {
                        max_attempts: 2,
                        initial_backoff_ms: 10,
                        max_backoff_ms: 100,
                    },
                    budget: None,
                },
                ExecutionNode {
                    id: "report".to_string(),
                    requirement_ids: vec![requirement_report.clone()],
                    depends_on: vec!["screen".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema,
                    operation: ExecutionOperation::Output {
                        value: json!({"report": format!("report-{suffix}")}),
                    },
                    retry: RetryPolicy {
                        max_attempts: 1,
                        initial_backoff_ms: 0,
                        max_backoff_ms: 0,
                    },
                    budget: None,
                },
            ],
        },
        run_input: json!({
            "years": 5,
            "definition": "artificial intelligence or AI"
        }),
    };
    ExecutionContractCaseV1 {
        schema_version: 1,
        case_id: format!("contract-{suffix}"),
        candidate,
        expected: ExecutionContractExpectationsV1 {
            requirements: vec![
                text_expectation(
                    "expected-screen",
                    &["every issuer", "five years", "citations"],
                ),
                text_expectation("expected-report", &["structured report", "source evidence"]),
            ],
            constraints: vec![
                text_expectation("expected-exclusions", &["exclude duplicate", "provenance"]),
                text_expectation(
                    "expected-definition",
                    &["define ai mentions", "artificial intelligence"],
                ),
            ],
            deliverables: vec![DeliverableExpectationV1 {
                expectation_id: "expected-deliverable".to_string(),
                output_pointer: "/report".to_string(),
                schema: report_schema,
            }],
            coverage: vec![CoverageExpectationV1 {
                expectation_id: "expected-coverage".to_string(),
                map_node_id: "screen".to_string(),
                expected_keys: keys,
                require_all: true,
            }],
            completion_checks: vec![
                CompletionCheckExpectationV1 {
                    expectation_id: "expected-check-coverage".to_string(),
                    kind: CompletionCheckKindExpectationV1::MapCoverage,
                    requirement_expectation_ids: vec!["expected-screen".to_string()],
                    constraint_expectation_ids: vec!["expected-exclusions".to_string()],
                },
                CompletionCheckExpectationV1 {
                    expectation_id: "expected-check-citations".to_string(),
                    kind: CompletionCheckKindExpectationV1::Citations,
                    requirement_expectation_ids: vec!["expected-report".to_string()],
                    constraint_expectation_ids: vec!["expected-definition".to_string()],
                },
            ],
            run_input: vec![
                RunInputExpectationV1 {
                    expectation_id: "expected-years".to_string(),
                    pointer: "/years".to_string(),
                    value: json!(5),
                },
                RunInputExpectationV1 {
                    expectation_id: "expected-definition-input".to_string(),
                    pointer: "/definition".to_string(),
                    value: json!("artificial intelligence or AI"),
                },
            ],
        },
        tags: vec![
            "bulk-universe".to_string(),
            "time-range".to_string(),
            "evidence-citations".to_string(),
            "exclusions".to_string(),
            "deliverables".to_string(),
            "definitions".to_string(),
            "multi-constraint".to_string(),
        ],
    }
}

fn text_expectation(expectation_id: &str, all_terms: &[&str]) -> TextExpectationV1 {
    TextExpectationV1 {
        expectation_id: expectation_id.to_string(),
        all_terms: all_terms.iter().map(|term| (*term).to_string()).collect(),
        any_terms: Vec::new(),
        forbidden_terms: Vec::new(),
    }
}
