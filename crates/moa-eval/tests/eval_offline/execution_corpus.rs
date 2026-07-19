//! Checked-in execution corpus integrity coverage.

use std::path::PathBuf;

use moa_core::types::execution_planning::ExecutionStrategy;
use moa_eval::execution::{ExecutionRoutingLabel, load_execution_corpus};

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml")
}

#[tokio::test]
async fn execution_corpus_loads_exact_counts_hashes_and_required_cohorts_offline() {
    // Pins: checked-in routing and contract rows cannot drift independently of the manifest.
    let corpus = load_execution_corpus(&manifest_path())
        .await
        .expect("checked-in execution corpus should load");

    assert_eq!(corpus.routing_cases.len(), 328);
    assert_eq!(corpus.contract_cases.len(), 80);
    assert_eq!(corpus.task_quality_cases.len(), 20);
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| case.expected_label == ExecutionRoutingLabel::Respond)
            .count(),
        60
    );
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| {
                case.expected_label == ExecutionRoutingLabel::Execute
                    && case.expected_strategy == Some(ExecutionStrategy::Inline)
            })
            .count(),
        144
    );
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| {
                case.expected_label == ExecutionRoutingLabel::Execute
                    && case.expected_strategy == Some(ExecutionStrategy::Durable)
            })
            .count(),
        104
    );
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| case.expected_label == ExecutionRoutingLabel::NeedsInput)
            .count(),
        20
    );
    // Enumerated parallel-workstream requests that forward-reference not-yet-provided user
    // material stay pinned to Execute/Durable (sessions S044/S072), never NeedsInput.
    let forward_reference_cases = corpus
        .routing_cases
        .iter()
        .filter(|case| {
            case.tags
                .iter()
                .any(|tag| tag == "parallel-workstream-forward-reference")
        })
        .collect::<Vec<_>>();
    assert_eq!(forward_reference_cases.len(), 4);
    assert!(forward_reference_cases.iter().all(|case| {
        case.expected_label == ExecutionRoutingLabel::Execute
            && case.expected_strategy == Some(ExecutionStrategy::Durable)
    }));
    assert!(corpus.routing_cases.iter().any(|case| {
        case.tags
            .iter()
            .any(|tag| tag == "sp500-ai-five-year-screen")
            && case.expected_label == ExecutionRoutingLabel::Execute
            && case.expected_strategy == Some(ExecutionStrategy::Durable)
    }));
    // Borderline skill-coverage requests (session S016) stay pinned to Execute/Inline and
    // always carry the covering installed skills the router sees as its coverage hint.
    let skill_coverage_cases = corpus
        .routing_cases
        .iter()
        .filter(|case| case.tags.iter().any(|tag| tag == "skill-coverage"))
        .collect::<Vec<_>>();
    assert_eq!(skill_coverage_cases.len(), 4);
    assert!(skill_coverage_cases.iter().all(|case| {
        case.expected_label == ExecutionRoutingLabel::Execute
            && case.expected_strategy == Some(ExecutionStrategy::Inline)
            && !case.available_skills.is_empty()
    }));
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| case.durable_upgrade.is_some())
            .count(),
        40
    );
}
