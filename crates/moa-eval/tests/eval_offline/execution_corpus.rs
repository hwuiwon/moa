//! Checked-in execution corpus integrity coverage.

use std::path::PathBuf;

use moa_core::types::execution_planning::ExecutionStrategy;
use moa_eval::execution::{ExecutionRoutingLabelV1, load_execution_corpus};

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml")
}

#[tokio::test]
async fn execution_corpus_loads_exact_counts_hashes_and_required_cohorts_offline() {
    // Pins: checked-in routing and contract rows cannot drift independently of the manifest.
    let corpus = load_execution_corpus(&manifest_path())
        .await
        .expect("checked-in execution corpus should load");

    assert_eq!(corpus.routing_cases.len(), 320);
    assert_eq!(corpus.contract_cases.len(), 80);
    assert_eq!(corpus.task_quality_cases.len(), 20);
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| case.expected_label == ExecutionRoutingLabelV1::Respond)
            .count(),
        60
    );
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| {
                case.expected_label == ExecutionRoutingLabelV1::Execute
                    && case.expected_strategy == Some(ExecutionStrategy::Inline)
            })
            .count(),
        140
    );
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| {
                case.expected_label == ExecutionRoutingLabelV1::Execute
                    && case.expected_strategy == Some(ExecutionStrategy::Durable)
            })
            .count(),
        100
    );
    assert_eq!(
        corpus
            .routing_cases
            .iter()
            .filter(|case| case.expected_label == ExecutionRoutingLabelV1::NeedsInput)
            .count(),
        20
    );
    assert!(corpus.routing_cases.iter().any(|case| {
        case.tags
            .iter()
            .any(|tag| tag == "sp500-ai-five-year-screen")
            && case.expected_label == ExecutionRoutingLabelV1::Execute
            && case.expected_strategy == Some(ExecutionStrategy::Durable)
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
