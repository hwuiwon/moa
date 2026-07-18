//! Deterministic execution goal-contract scoring coverage.

use std::path::PathBuf;

use moa_artifacts::execution_plan::ExecutionRequirement;
use moa_eval::execution::{TextExpectation, load_execution_corpus, score_contract_case};

#[tokio::test]
async fn execution_contract_recorded_candidates_match_every_gold_category_offline() {
    // Pins: all recorded strict candidates preserve every independently stated contract entry.
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml");
    let corpus = load_execution_corpus(&manifest)
        .await
        .expect("checked-in execution corpus should load");

    for case in &corpus.contract_cases {
        let score = score_contract_case(case).expect("recorded contract case should score");
        assert_eq!(score.macro_f1, 1.0, "{}", case.case_id);
        assert!(!score.contract_omission, "{}", case.case_id);
    }
}

#[tokio::test]
async fn execution_contract_omission_and_unsupported_entry_lower_recall_and_precision_offline() {
    // Pins: deterministic scoring detects both a dropped user requirement and invented scope.
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml");
    let corpus = load_execution_corpus(&manifest)
        .await
        .expect("checked-in execution corpus should load");
    let base = corpus
        .contract_cases
        .first()
        .expect("contract corpus should be non-empty");

    let mut omitted = base.clone();
    omitted.candidate.goal.requirements.pop();
    let omitted_score = score_contract_case(&omitted).expect("omitted case should score");
    assert!(omitted_score.contract_omission);
    assert!(omitted_score.requirements.metrics.recall < 1.0);

    let mut invented = base.clone();
    invented
        .candidate
        .goal
        .requirements
        .push(ExecutionRequirement {
            id: "invented-scope".to_string(),
            description: "Analyze an unsupported unrelated market universe".to_string(),
        });
    let invented_score = score_contract_case(&invented).expect("invented case should score");
    assert_eq!(invented_score.requirements.metrics.recall, 1.0);
    assert!(invented_score.requirements.metrics.precision < 1.0);
}

#[tokio::test]
async fn execution_contract_one_actual_entry_cannot_satisfy_two_gold_entries_offline() {
    // Pins: maximum matching is one-to-one; overlapping gold language cannot reuse one actual row.
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml");
    let corpus = load_execution_corpus(&manifest)
        .await
        .expect("checked-in execution corpus should load");
    let mut case = corpus.contract_cases[0].clone();
    case.candidate.goal.requirements.truncate(1);
    case.candidate.goal.completion_checks.clear();
    case.expected.completion_checks.clear();
    case.expected.requirements = vec![
        TextExpectation {
            expectation_id: "overlap-a".to_string(),
            all_terms: vec!["every issuer".to_string()],
            any_terms: Vec::new(),
            forbidden_terms: Vec::new(),
        },
        TextExpectation {
            expectation_id: "overlap-b".to_string(),
            all_terms: vec!["five years".to_string()],
            any_terms: Vec::new(),
            forbidden_terms: Vec::new(),
        },
    ];

    let score = score_contract_case(&case).expect("overlapping case should score");
    assert_eq!(score.requirements.metrics.expected_count, 2);
    assert_eq!(score.requirements.metrics.actual_count, 1);
    assert_eq!(score.requirements.metrics.matched_count, 1);
    assert_eq!(score.requirements.metrics.recall, 0.5);
    assert!(score.contract_omission);
}
