//! Production-path offline execution-routing corpus coverage.

use std::path::PathBuf;

use moa_brain::execution_planning::{
    ExecutionRouteClassifierLabel, ExecutionRouteClassifierOutput,
};
use moa_core::types::{
    completion::TokenUsage,
    execution_planning::{
        DurableUpgradeTransitionError, ExecutionRouteClassifierOutcome, ExecutionRouteDecision,
        ExecutionStrategy, durable_upgrade_transition,
    },
};
use moa_eval::execution::{
    ExecutionRoutingCase, ExecutionRoutingClassifierFixture, ExecutionRoutingLabel,
    load_execution_corpus, score_routing_cases,
};

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml")
}

fn response_case(
    case_id: &str,
    expected_strategy: ExecutionStrategy,
    observed_label: ExecutionRouteClassifierLabel,
    observed_strategy: Option<ExecutionStrategy>,
    confidence_bps: u16,
    expected_outcome: ExecutionRouteClassifierOutcome,
) -> ExecutionRoutingCase {
    ExecutionRoutingCase {
        schema_version: 1,
        case_id: case_id.to_string(),
        objective: "Evaluate the supplied work exactly".to_string(),
        attachment_count: 0,
        has_recent_target: false,
        available_skills: Vec::new(),
        classifier: ExecutionRoutingClassifierFixture::Response {
            output: ExecutionRouteClassifierOutput {
                label: observed_label,
                strategy: observed_strategy,
                rationale: "A domain-specific workflow supports this route and strategy."
                    .to_string(),
                confidence_bps,
                missing_inputs: Vec::new(),
            },
            usage: TokenUsage::default(),
            cost_microusd: 0,
        },
        expected_classifier_outcome: expected_outcome,
        expected_label: ExecutionRoutingLabel::Execute,
        expected_strategy: Some(expected_strategy),
        near_boundary: false,
        durable_upgrade: None,
        expected_durable_upgrade_evidence: None,
        tags: Vec::new(),
    }
}

#[tokio::test]
async fn execution_routing_scores_decision_strategy_and_upgrade_separately_offline() {
    // Pins: the complete corpus drives the production router and shared Durable-upgrade
    // transition with exact public-route, strategy, fallback, and evidence metrics.
    let corpus = load_execution_corpus(&manifest_path())
        .await
        .expect("checked-in execution corpus should load");
    let metrics = score_routing_cases(&corpus.routing_cases)
        .await
        .expect("scripted routing corpus should score");

    assert_eq!(metrics.total_cases, 328);
    assert_eq!(metrics.passed_cases, 328);
    assert_eq!(metrics.weighted_routing_cost_total, 0);
    assert_eq!(metrics.weighted_strategy_cost_total, 0);
    assert_eq!(metrics.respond_on_execute_count, 0);
    assert_eq!(metrics.respond_on_execute_rate, 0.0);
    assert_eq!(metrics.near_boundary_inline_recall, 1.0);
    assert_eq!(metrics.durable_strategy_recall, 1.0);
    assert_eq!(metrics.durable_upgrade_recall, 1.0);
    assert_eq!(metrics.durable_upgrade_evidence_preservation_rate, 1.0);
    assert_eq!(metrics.needs_input_false_accept_rate, 0.0);
    assert_eq!(metrics.unnecessary_clarification_rate, 0.0);
    for outcome in [
        "provider_error",
        "stream_error",
        "schema_rejected",
        "oversized",
        "low_confidence",
        "invalid_decision",
    ] {
        assert_eq!(metrics.classifier_fallback_counts.get(outcome), Some(&4));
    }
    assert_eq!(
        metrics
            .classifier_fallback_counts
            .get("context_forced_inline"),
        Some(&8)
    );
    assert_eq!(metrics.classifier_fallback_rate, 32.0 / 288.0);
    let upgrades = metrics
        .cases
        .iter()
        .filter(|case| case.durable_upgrade_evidence_preserved.is_some())
        .collect::<Vec<_>>();
    assert_eq!(upgrades.len(), 40);
    assert!(upgrades.iter().all(|case| {
        case.classifier_calls == 0 && case.durable_upgrade_evidence_preserved == Some(true)
    }));
}

#[tokio::test]
async fn execution_routing_costs_pin_catastrophe_and_strategy_direction_offline() {
    // Pins: Execute-to-Respond stays catastrophic while strategy over- and
    // under-execution remain separate, direction-sensitive costs and exact
    // Execute strategy matches remain comparable at zero cost.
    let cases = vec![
        response_case(
            "execute-respond-catastrophe",
            ExecutionStrategy::Durable,
            ExecutionRouteClassifierLabel::Respond,
            None,
            9_500,
            ExecutionRouteClassifierOutcome::Accepted,
        ),
        response_case(
            "inline-durable-over-execution",
            ExecutionStrategy::Inline,
            ExecutionRouteClassifierLabel::Execute,
            Some(ExecutionStrategy::Durable),
            9_500,
            ExecutionRouteClassifierOutcome::Accepted,
        ),
        response_case(
            "durable-inline-under-execution",
            ExecutionStrategy::Durable,
            ExecutionRouteClassifierLabel::Execute,
            Some(ExecutionStrategy::Inline),
            9_500,
            ExecutionRouteClassifierOutcome::Accepted,
        ),
        response_case(
            "inline-inline-exact-match",
            ExecutionStrategy::Inline,
            ExecutionRouteClassifierLabel::Execute,
            Some(ExecutionStrategy::Inline),
            9_500,
            ExecutionRouteClassifierOutcome::Accepted,
        ),
        response_case(
            "durable-durable-exact-match",
            ExecutionStrategy::Durable,
            ExecutionRouteClassifierLabel::Execute,
            Some(ExecutionStrategy::Durable),
            9_500,
            ExecutionRouteClassifierOutcome::Accepted,
        ),
    ];
    let metrics = score_routing_cases(&cases)
        .await
        .expect("cost probe cases should score");

    assert_eq!(metrics.cases[0].routing_cost, 50);
    assert_eq!(metrics.cases[0].strategy_cost, None);
    assert_eq!(metrics.cases[1].routing_cost, 0);
    assert_eq!(metrics.cases[1].strategy_cost, Some(4));
    assert_eq!(metrics.cases[2].routing_cost, 0);
    assert_eq!(metrics.cases[2].strategy_cost, Some(8));
    assert_eq!(metrics.cases[3].routing_cost, 0);
    assert_eq!(metrics.cases[3].strategy_cost, Some(0));
    assert_eq!(metrics.cases[4].routing_cost, 0);
    assert_eq!(metrics.cases[4].strategy_cost, Some(0));
}

#[tokio::test]
async fn execution_routing_low_confidence_durable_strategy_falls_back_inline_offline() {
    // Pins: an untrusted Durable recommendation below the confidence boundary
    // uses the production Execute/Inline fallback instead of starting a run.
    let case = response_case(
        "low-confidence-durable-fallback",
        ExecutionStrategy::Inline,
        ExecutionRouteClassifierLabel::Execute,
        Some(ExecutionStrategy::Durable),
        7_999,
        ExecutionRouteClassifierOutcome::LowConfidence,
    );
    let metrics = score_routing_cases(&[case])
        .await
        .expect("low-confidence case should score");
    assert_eq!(metrics.passed_cases, 1);
    assert_eq!(
        metrics.cases[0].observed_strategy,
        Some(ExecutionStrategy::Inline)
    );
    assert_eq!(metrics.cases[0].classifier_calls, 1);
}

#[tokio::test]
async fn execution_routing_scores_handoff_evidence_and_rejects_classifier_setup_offline() {
    // Pins: evidence preservation is scored from the production transition output rather than
    // accepted tautologically as corpus setup, while upgrades still cannot call the classifier.
    let corpus = load_execution_corpus(&manifest_path())
        .await
        .expect("checked-in execution corpus should load");
    let upgrade = corpus
        .routing_cases
        .iter()
        .find(|case| case.durable_upgrade.is_some())
        .expect("corpus should contain a Durable-upgrade case")
        .clone();

    let mut corrupt_evidence = upgrade.clone();
    corrupt_evidence
        .expected_durable_upgrade_evidence
        .as_mut()
        .expect("upgrade should pin evidence")
        .push(
            moa_core::types::execution_planning::ExecutionPlanningEvidence {
                source: "corrupt".to_string(),
                summary: "not emitted by the production transition".to_string(),
                value: serde_json::json!({"corrupt": true}),
            },
        );
    let corrupt_metrics = score_routing_cases(&[corrupt_evidence])
        .await
        .expect("mismatched expected evidence should be scored, not rejected as setup");
    assert_eq!(corrupt_metrics.passed_cases, 0);
    assert_eq!(
        corrupt_metrics.durable_upgrade_evidence_preservation_rate,
        0.0
    );
    assert_eq!(
        corrupt_metrics.cases[0].durable_upgrade_evidence_preserved,
        Some(false)
    );

    let mut classifier_setup = upgrade;
    classifier_setup.classifier = ExecutionRoutingClassifierFixture::Response {
        output: ExecutionRouteClassifierOutput {
            label: ExecutionRouteClassifierLabel::Execute,
            strategy: Some(ExecutionStrategy::Durable),
            rationale: "The workflow must survive a delayed approval.".to_string(),
            confidence_bps: 9_500,
            missing_inputs: Vec::new(),
        },
        usage: TokenUsage::default(),
        cost_microusd: 0,
    };
    assert!(score_routing_cases(&[classifier_setup]).await.is_err());
}

#[tokio::test]
async fn execution_routing_durable_upgrade_rejects_non_root_and_reuse_offline() {
    // Pins: the eval's real Durable-upgrade transition remains root-only and one-way.
    let corpus = load_execution_corpus(&manifest_path())
        .await
        .expect("checked-in execution corpus should load");
    let upgrade = corpus
        .routing_cases
        .iter()
        .find_map(|case| case.durable_upgrade.as_ref())
        .expect("corpus should contain a Durable-upgrade signal");
    let initial_route = ExecutionRouteDecision::Execute {
        strategy: ExecutionStrategy::Inline,
        rationale: "The work can begin in a bounded interactive loop.".to_string(),
    };

    assert_eq!(
        durable_upgrade_transition(
            &upgrade.objective,
            &initial_route,
            false,
            false,
            upgrade.clone(),
        ),
        Err(DurableUpgradeTransitionError::NotAuthorized)
    );
    assert_eq!(
        durable_upgrade_transition(
            &upgrade.objective,
            &initial_route,
            true,
            true,
            upgrade.clone(),
        ),
        Err(DurableUpgradeTransitionError::AlreadyConsumed)
    );
}
