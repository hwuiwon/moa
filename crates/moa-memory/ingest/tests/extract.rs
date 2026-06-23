//! Out-of-line tests for deterministic fact extraction.

use moa_memory_ingest::{
    FactExtractor, HeuristicFactExtractor, IngestError, ScriptedFactExtractor, TurnChunk,
    extract_facts, extract_facts_checked, extraction_confidence_hint,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ExpectedFacts {
    facts: Vec<ExpectedFact>,
}

#[derive(Debug, Deserialize)]
struct ExpectedFact {
    subject: String,
    predicate: String,
    object: String,
}

fn chunk(text: &str) -> TurnChunk {
    TurnChunk {
        index: 0,
        text: text.to_string(),
        token_estimate: 1,
    }
}

#[test]
fn extract_emits_expected_facts_from_canonical_paragraph_describing_a_service() {
    let expected: ExpectedFacts =
        serde_json::from_str(include_str!("support/fixtures/expected_facts_simple.json"))
            .expect("expected fact fixture parses");
    let facts = extract_facts(&[chunk(include_str!("support/fixtures/document_simple.md"))]);

    assert_eq!(facts.len(), expected.facts.len());
    for (fact, expected) in facts.iter().zip(expected.facts.iter()) {
        assert_eq!(fact.subject, expected.subject);
        assert_eq!(fact.predicate, expected.predicate);
        assert_eq!(fact.object, expected.object);
    }
}

#[tokio::test]
async fn heuristic_extractor_matches_deterministic_extract_facts_output() {
    // Pins: the default extractor preserves deterministic extraction behavior.
    let chunks = [chunk(
        "Fact: API runs_on_port 3000\nFact: worker_queue uses Redis",
    )];
    let expected = extract_facts(&chunks);

    let facts = HeuristicFactExtractor
        .extract(&chunks)
        .await
        .expect("heuristic extractor should not fail");

    assert_eq!(facts, expected);
}

#[tokio::test]
async fn scripted_extractor_can_emit_fact_for_text_skipped_by_heuristic() {
    // Pins: scripted extraction can supply corpus facts for non-declarative transcript text.
    let chunks = [chunk("Should we use Redis? Please review the design.")];
    assert_eq!(extract_facts(&chunks), Vec::new());
    let extractor = ScriptedFactExtractor::from_summaries(["planner chooses Redis"]);

    let facts = extractor
        .extract(&chunks)
        .await
        .expect("scripted extractor should not fail");

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].subject, "planner");
    assert_eq!(facts[0].predicate, "chooses");
    assert_eq!(facts[0].object, "Redis");
    assert_eq!(facts[0].summary, "planner chooses Redis");
    assert_eq!(facts[0].source_chunk, 0);
}

#[test]
fn extract_assigns_confidence_scores_consistent_with_text_qualifiers() {
    let hedged = extraction_confidence_hint("system probably uses JWT");
    let definitive = extraction_confidence_hint("system uses JWT");

    assert_eq!(hedged, 0.45);
    assert_eq!(definitive, 0.70);
    assert!(hedged < definitive);
}

#[test]
fn extract_handles_negation_correctly_in_emitted_facts() {
    let facts = extract_facts(&[chunk("Fact: API does_NOT_support batch_requests")]);

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].subject, "API");
    assert_eq!(facts[0].predicate, "does_NOT_support");
    assert_eq!(facts[0].object, "batch_requests");
}

#[test]
fn extract_strips_marked_fact_is_connector_from_object() {
    // Pins: marked corpus facts connect dependency objects to ownership subjects through one entity.
    let facts = extract_facts(&[chunk(
        "Fact: tenant shared audit-shipper-dep-test depends_on is lib-audit-wire-test.",
    )]);

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].subject, "audit-shipper-dep-test");
    assert_eq!(facts[0].predicate, "depends_on");
    assert_eq!(facts[0].object, "lib-audit-wire-test.");
}

#[test]
fn extract_skips_questions_and_imperatives_yielding_no_facts() {
    let facts = extract_facts(&[chunk("Should we use Redis? Please review the design.")]);

    assert!(facts.is_empty(), "questions and imperatives are not facts");
}

#[test]
fn extract_returns_typed_error_for_chunks_exceeding_max_size() {
    let oversized = TurnChunk {
        index: 3,
        text: "x".repeat(50_000),
        token_estimate: 12_500,
    };

    let error = extract_facts_checked(&[oversized]).expect_err("oversized chunk must fail");

    assert!(matches!(
        error,
        IngestError::ChunkTooLarge {
            index: 3,
            actual_chars: 50_000,
            ..
        }
    ));
}
