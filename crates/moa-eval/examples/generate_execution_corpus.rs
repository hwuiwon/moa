//! Regenerates the checked-in deterministic execution routing and contract corpora.

use std::{fs, path::PathBuf};

use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionConstraint, ExecutionDeliverable, ExecutionGoalContract, ExecutionNode,
    ExecutionOperation, ExecutionPlanDefinition, ExecutionRequirement, GeneratedExecutionCandidate,
    MapTask, RetryPolicy,
};
use moa_brain::execution_planning::{
    ExecutionRouteClassifierLabel, ExecutionRouteClassifierOutput,
};
use moa_core::types::{
    completion::TokenUsage,
    execution_planning::{
        DurableUpgradeSignal, ExecutionPlanningEvidence, ExecutionRouteClassifierOutcome,
        ExecutionStrategy,
    },
};
use moa_eval::execution::{
    CompletionCheckExpectation, CoverageExpectation, DeliverableExpectation, ExecutionContractCase,
    ExecutionContractExpectations, ExecutionRoutingCase, ExecutionRoutingClassifierFixture,
    ExecutionRoutingLabel, RunInputExpectation, TextExpectation,
    contract::CompletionCheckKindExpectation,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution");
    fs::create_dir_all(&root)?;
    let routing = jsonl(&routing_cases())?;
    let contract = jsonl(&contract_cases())?;
    let task_quality = fs::read(root.join("task-quality.jsonl"))?;
    fs::write(root.join("routing.jsonl"), &routing)?;
    fs::write(root.join("contract-recorded.jsonl"), &contract)?;
    let manifest = format!(
        "schema_version = 1\n\n[routing]\npath = \"routing.jsonl\"\nsha256 = \"{}\"\ncount = 328\n\n[contract]\npath = \"contract-recorded.jsonl\"\nsha256 = \"{}\"\ncount = 80\n\n[task_quality]\npath = \"task-quality.jsonl\"\nsha256 = \"{}\"\ncount = 20\n",
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

fn routing_cases() -> Vec<ExecutionRoutingCase> {
    let mut cases = Vec::with_capacity(328);
    for index in 0..60 {
        cases.push(route_case(
            format!("respond-{index:03}"),
            format!("Explain the stable concept numbered {index} in one concise answer."),
            ExecutionRoutingLabel::Respond,
            None,
            response_fixture(
                ExecutionRouteClassifierLabel::Respond,
                None,
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
            ExecutionRoutingLabel::Execute,
            Some(ExecutionStrategy::Inline),
            response_fixture(
                ExecutionRouteClassifierLabel::Execute,
                Some(ExecutionStrategy::Inline),
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
                    case.classifier = ExecutionRoutingClassifierFixture::ProviderError;
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::ProviderError;
                }
                1 => {
                    case.classifier = ExecutionRoutingClassifierFixture::StreamError;
                    case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::StreamError;
                }
                2 => {
                    case.classifier = ExecutionRoutingClassifierFixture::Malformed;
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::SchemaRejected;
                }
                3 => {
                    case.classifier = ExecutionRoutingClassifierFixture::Oversized;
                    case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::Oversized;
                }
                4 => {
                    case.classifier = response_fixture(
                        ExecutionRouteClassifierLabel::Execute,
                        Some(ExecutionStrategy::Durable),
                        7_999,
                        Vec::new(),
                    );
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::LowConfidence;
                }
                _ => {
                    case.classifier = response_fixture(
                        ExecutionRouteClassifierLabel::Respond,
                        Some(ExecutionStrategy::Durable),
                        9_500,
                        Vec::new(),
                    );
                    case.expected_classifier_outcome =
                        ExecutionRouteClassifierOutcome::InvalidDecision;
                }
            }
        } else if (104..112).contains(&index) {
            case.classifier = response_fixture(
                ExecutionRouteClassifierLabel::Respond,
                None,
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
            ExecutionRoutingLabel::Execute,
            Some(ExecutionStrategy::Durable),
            response_fixture(
                ExecutionRouteClassifierLabel::Execute,
                Some(ExecutionStrategy::Durable),
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
            case.classifier = ExecutionRoutingClassifierFixture::NotCalled;
            case.expected_classifier_outcome = ExecutionRouteClassifierOutcome::NotCalled;
            case.durable_upgrade = Some(DurableUpgradeSignal {
                objective,
                rationale: format!(
                    "The discovered workflow {index} must continue durably across independent work items."
                ),
                evidence: evidence.clone(),
            });
            case.expected_durable_upgrade_evidence = Some(evidence);
            case.tags.push("durable-upgrade".to_string());
        }
        cases.push(case);
    }
    // Enumerated parallel-workstream requests that forward-reference user material not yet
    // provided must still route to Execute/Durable: the coordinator delegates one worker per
    // workstream now (building the framework for any blocked workstream) instead of answering
    // directly or asking for the pending inputs. Pins the model-driven regression from the live
    // 100-session sweep (sessions S044/S072) at the deterministic routing boundary.
    for (suffix, objective) in parallel_workstream_forward_reference_cases() {
        let mut case = route_case(
            format!("execute-durable-parallel-forward-ref-{suffix}"),
            objective.to_string(),
            ExecutionRoutingLabel::Execute,
            Some(ExecutionStrategy::Durable),
            response_fixture(
                ExecutionRouteClassifierLabel::Execute,
                Some(ExecutionStrategy::Durable),
                9_500,
                Vec::new(),
            ),
            ExecutionRouteClassifierOutcome::Accepted,
        );
        case.tags
            .push("parallel-workstream-forward-reference".to_string());
        case.tags.push(suffix.to_string());
        cases.push(case);
    }
    // Borderline requests that a covering installed skill should carry to
    // Execute/Inline rather than stalling in NeedsInput: the skill supplies its own
    // guidance for identifying missing inputs without blocking. Pins the model-driven
    // regression from the live sweep (session S016) at the deterministic routing
    // boundary, with the covering skills offered as the router's coverage hint.
    for (suffix, objective, skills) in skill_coverage_cases() {
        let mut case = route_case(
            format!("execute-inline-skill-coverage-{suffix}"),
            objective.to_string(),
            ExecutionRoutingLabel::Execute,
            Some(ExecutionStrategy::Inline),
            response_fixture(
                ExecutionRouteClassifierLabel::Execute,
                Some(ExecutionStrategy::Inline),
                9_000,
                Vec::new(),
            ),
            ExecutionRouteClassifierOutcome::Accepted,
        );
        case.available_skills = skills.iter().map(|skill| (*skill).to_string()).collect();
        case.tags.push("skill-coverage".to_string());
        case.tags.push(suffix.to_string());
        cases.push(case);
    }
    for index in 0..20 {
        let mut case = route_case(
            format!("needs-input-{index:03}"),
            format!("Screen the requested dataset {index}."),
            ExecutionRoutingLabel::NeedsInput,
            None,
            response_fixture(
                ExecutionRouteClassifierLabel::NeedsInput,
                None,
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

/// Borderline objectives paired with the installed skills that plausibly cover them.
/// Each must route to Execute/Inline instead of NeedsInput because a covering skill
/// carries its own guidance for gathering any missing inputs without blocking.
fn skill_coverage_cases() -> [(&'static str, &'static str, &'static [&'static str]); 4] {
    [
        (
            "refund-policy-summary",
            "Summarize our refund policy into five plain-language bullets.",
            &["refund-triage", "policy-drafting", "customer-comms"],
        ),
        (
            "onboarding-checklist",
            "Turn our onboarding guide into a checklist a new hire can follow.",
            &["employee-onboarding", "doc-formatting"],
        ),
        (
            "incident-postmortem",
            "Draft a postmortem for last night's outage from what we discussed.",
            &["incident-review", "postmortem-writing", "timeline-builder"],
        ),
        (
            "invoice-reconciliation",
            "Reconcile this month's invoices against the purchase orders.",
            &["invoice-reconciliation", "finance-ops"],
        ),
    ]
}

/// Enumerated parallel-workstream objectives whose workstreams forward-reference user
/// material that has not been provided yet. Each must route to Execute/Durable so the
/// coordinator delegates a worker per workstream instead of responding or asking for input.
fn parallel_workstream_forward_reference_cases() -> [(&'static str, &'static str); 4] {
    [
        (
            "soc2-evidence-gaps",
            "Split the SOC 2 audit prep into parallel workstreams: one worker assesses the evidence gaps from the control list I will share, another drafts the remediation timeline, and another maps owner assignments, then combine them into one readiness memo.",
        ),
        (
            "handoff-mapping",
            "Run these in parallel: one worker maps the team handoffs from the notes I will paste, one worker builds the RACI matrix, and one worker lists the escalation paths, and give me a single coordination plan.",
        ),
        (
            "vendor-review",
            "1) One worker reviews the vendor security questionnaire responses from the spreadsheet I will attach, 2) another benchmarks pricing across the shortlisted vendors, 3) another summarizes the contract red flags, then synthesize a recommendation.",
        ),
        (
            "launch-readiness",
            "In parallel, have one worker assess the launch blockers from the checklist I will send, one worker draft the go-to-market brief, and one worker prepare the rollback plan, and merge them into a launch-readiness summary.",
        ),
    ]
}

fn route_case(
    case_id: String,
    objective: String,
    expected_label: ExecutionRoutingLabel,
    expected_strategy: Option<ExecutionStrategy>,
    classifier: ExecutionRoutingClassifierFixture,
    expected_classifier_outcome: ExecutionRouteClassifierOutcome,
) -> ExecutionRoutingCase {
    ExecutionRoutingCase {
        schema_version: 1,
        case_id,
        objective,
        attachment_count: 0,
        has_recent_target: false,
        available_skills: Vec::new(),
        classifier,
        expected_classifier_outcome,
        expected_label,
        expected_strategy,
        near_boundary: false,
        durable_upgrade: None,
        expected_durable_upgrade_evidence: None,
        tags: Vec::new(),
    }
}

fn response_fixture(
    label: ExecutionRouteClassifierLabel,
    strategy: Option<ExecutionStrategy>,
    confidence_bps: u16,
    missing_inputs: Vec<String>,
) -> ExecutionRoutingClassifierFixture {
    ExecutionRoutingClassifierFixture::Response {
        output: ExecutionRouteClassifierOutput {
            label,
            strategy,
            rationale: match (label, strategy) {
                (ExecutionRouteClassifierLabel::Respond, None) => {
                    "The request only requires a direct explanatory response."
                }
                (ExecutionRouteClassifierLabel::Execute, Some(ExecutionStrategy::Inline)) => {
                    "The requested work fits a bounded interactive execution loop."
                }
                (ExecutionRouteClassifierLabel::Execute, Some(ExecutionStrategy::Durable)) => {
                    "The requested workflow should persist as a durable execution."
                }
                (ExecutionRouteClassifierLabel::NeedsInput, None) => {
                    "A concrete coverage universe is required before work can begin."
                }
                _ => "The declared route and strategy are intentionally inconsistent.",
            }
            .to_string(),
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

fn contract_cases() -> Vec<ExecutionContractCase> {
    (0..80).map(contract_case).collect()
}

fn contract_case(index: usize) -> ExecutionContractCase {
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
            schema_version: 2,
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
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
                    compensation: None,
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
                    compensation: None,
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
    ExecutionContractCase {
        schema_version: 1,
        case_id: format!("contract-{suffix}"),
        candidate,
        expected: ExecutionContractExpectations {
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
            deliverables: vec![DeliverableExpectation {
                expectation_id: "expected-deliverable".to_string(),
                output_pointer: "/report".to_string(),
                schema: report_schema,
            }],
            coverage: vec![CoverageExpectation {
                expectation_id: "expected-coverage".to_string(),
                map_node_id: "screen".to_string(),
                expected_keys: keys,
                require_all: true,
            }],
            completion_checks: vec![
                CompletionCheckExpectation {
                    expectation_id: "expected-check-coverage".to_string(),
                    kind: CompletionCheckKindExpectation::MapCoverage,
                    requirement_expectation_ids: vec!["expected-screen".to_string()],
                    constraint_expectation_ids: vec!["expected-exclusions".to_string()],
                },
                CompletionCheckExpectation {
                    expectation_id: "expected-check-citations".to_string(),
                    kind: CompletionCheckKindExpectation::Citations,
                    requirement_expectation_ids: vec!["expected-report".to_string()],
                    constraint_expectation_ids: vec!["expected-definition".to_string()],
                },
            ],
            run_input: vec![
                RunInputExpectation {
                    expectation_id: "expected-years".to_string(),
                    pointer: "/years".to_string(),
                    value: json!(5),
                },
                RunInputExpectation {
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

fn text_expectation(expectation_id: &str, all_terms: &[&str]) -> TextExpectation {
    TextExpectation {
        expectation_id: expectation_id.to_string(),
        all_terms: all_terms.iter().map(|term| (*term).to_string()).collect(),
        any_terms: Vec::new(),
        forbidden_terms: Vec::new(),
    }
}
