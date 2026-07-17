//! Production-path offline execution-routing corpus coverage.

use std::path::PathBuf;

use moa_eval::execution::{load_execution_corpus, score_routing_cases};

#[tokio::test]
async fn execution_routing_scores_scripted_classifier_without_respond_on_run_offline() {
    // Pins: the complete corpus drives the async production router and conservative fallback
    // policy, with Respond-on-Run held at exactly zero.
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/execution/manifest.toml");
    let corpus = load_execution_corpus(&manifest)
        .await
        .expect("checked-in execution corpus should load");
    let metrics = score_routing_cases(&corpus.routing_cases)
        .await
        .expect("scripted routing corpus should score");

    assert_eq!(metrics.total_cases, 320);
    assert_eq!(metrics.passed_cases, 320);
    assert_eq!(metrics.respond_on_run_count, 0);
    assert_eq!(metrics.respond_on_run_rate, 0.0);
    assert_eq!(metrics.near_boundary_act_recall, 1.0);
    assert_eq!(metrics.escalation_recall, 1.0);
    assert_eq!(metrics.escalation_evidence_preservation_rate, 1.0);
    assert_eq!(metrics.needs_input_false_accept_rate, 0.0);
    assert_eq!(metrics.unnecessary_clarification_rate, 0.0);
    assert_eq!(
        metrics.classifier_fallback_counts.get("provider_error"),
        Some(&4)
    );
    assert_eq!(
        metrics.classifier_fallback_counts.get("stream_error"),
        Some(&4)
    );
    assert_eq!(
        metrics.classifier_fallback_counts.get("schema_rejected"),
        Some(&4)
    );
    assert_eq!(
        metrics.classifier_fallback_counts.get("oversized"),
        Some(&4)
    );
    assert_eq!(
        metrics.classifier_fallback_counts.get("low_confidence"),
        Some(&4)
    );
    assert_eq!(
        metrics.classifier_fallback_counts.get("invalid_decision"),
        Some(&4)
    );
    assert_eq!(
        metrics.classifier_fallback_counts.get("context_forced_act"),
        Some(&8)
    );
    assert_eq!(metrics.classifier_fallback_rate, 32.0 / 280.0);
    assert!(metrics.classifier_tokens_per_routed_turn > 0.0);
    assert!(metrics.classifier_cost_microusd_per_routed_turn > 0.0);
    assert!(metrics.classifier_latency_ms_per_routed_turn >= 0.0);
}

#[test]
fn execution_routing_respond_on_run_cost_is_catastrophic_offline() {
    // Pins: mutation of the asymmetric catastrophe cell from 50 to zero must fail this guard.
    let source = include_str!("../../src/execution/routing.rs");
    assert!(source.contains("(Respond, Run) => 50"));
}
