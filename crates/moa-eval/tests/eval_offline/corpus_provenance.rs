//! Lane classification, corpus leakage, and cohort pairing over real corpora.
//!
//! The kernel unit tests prove the scanner's rules. These tests apply them to the
//! corpora that actually gate, and pin the classification decisions that decide
//! *which* controls a lane gets.

use std::collections::BTreeSet;

use moa_eval::controls::{
    LANE_CLASSIFICATIONS, SUITE_EXECUTION_ROUTING, SUITE_EXTERNAL_MEMORY, SUITE_GOLDEN_GRAPH,
    SUITE_LONG_CONVERSATION, SUITE_MEMORY_RETRIEVAL, SUITE_WIXQA_RAG, lane_classification,
};
use moa_eval::kernel::cohorts::{AnchorCohort, CohortError, PairedRunIdentity, require_paired};
use moa_eval::kernel::contamination::{
    ArtifactKind, CaseSplit, ContaminationError, CorpusObject, EvalCaseText, LaneClass,
    LeakageFinding, LeakageScanner, PinnedCorpus, SourceProvenance,
};
use moa_eval::memory_eval::{CorpusProfile, TranscriptStyle, generate_memory_eval_corpus};

use chrono::{TimeZone, Utc};
use sha2::{Digest, Sha256};

fn sha256_of(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn provenance(source: &str) -> Option<SourceProvenance> {
    Some(SourceProvenance {
        source_uri: source.to_string(),
        upstream_revision: "generated".to_string(),
        retrieved_at: Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0).unwrap(),
    })
}

#[test]
fn every_eval_lane_is_classified_and_internally_consistent() {
    // Pins: the classification table covers every suite that has controls, and no
    // entry contradicts itself (a pinned corpus that allows network).
    for lane in [
        SUITE_MEMORY_RETRIEVAL,
        SUITE_GOLDEN_GRAPH,
        SUITE_WIXQA_RAG,
        SUITE_EXTERNAL_MEMORY,
        SUITE_EXECUTION_ROUTING,
        SUITE_LONG_CONVERSATION,
    ] {
        let classification =
            lane_classification(lane).unwrap_or_else(|| panic!("{lane} classified"));
        classification
            .validate()
            .unwrap_or_else(|error| panic!("{lane}: {error}"));
    }
    assert_eq!(LANE_CLASSIFICATIONS.len(), 6);
}

#[test]
fn a_closed_fixture_suite_needs_no_corpus_leakage_scan() {
    // Pins: the execution routing corpus *is* the case set, questions and labels
    // included. Scanning it as a retrieval corpus would report leakage by
    // construction, which is why the lane is classified as a closed fixture suite
    // instead.
    let routing = lane_classification(SUITE_EXECUTION_ROUTING).expect("classified");
    assert_eq!(routing.class, LaneClass::ClosedFixtureSuite);
    assert!(!routing.requires_leakage_scan());
    let retrieval = lane_classification(SUITE_MEMORY_RETRIEVAL).expect("classified");
    assert!(retrieval.requires_leakage_scan());
}

#[test]
fn wixqa_gets_package_leakage_controls() {
    // Pins: the closed-corpus contract is asserted at the lane table.
    let wixqa = lane_classification(SUITE_WIXQA_RAG).expect("classified");
    assert_eq!(wixqa.class, LaneClass::FixedCorpusRetrieval);
    assert!(wixqa.network_denied);
    assert!(wixqa.requires_leakage_scan());
}

#[test]
fn a_wixqa_shaped_corpus_passes_while_a_seeded_answer_key_fails_closed() {
    // Pins: the distinction that matters for a fixed-corpus RAG lane. A support
    // article that answers the question is expected; a page pairing the question
    // with its answer key is leakage.
    let article = "Rotating a signing key requires opening the console, choosing security, \
        and choosing rotate. The rotation window is twenty four hours and cannot be shortened.";
    let cases = vec![EvalCaseText {
        case_id: "wix-001".to_string(),
        split: CaseSplit::GatedTest,
        question: "How long is the signing key rotation window?".to_string(),
        answer: "The rotation window is twenty four hours".to_string(),
    }];
    let clean = vec![CorpusObject {
        object_id: "kb-001".to_string(),
        declared_kind: ArtifactKind::SourceDocument,
        content_sha256: Some(sha256_of(article)),
        provenance: provenance("https://support.example.test/kb-001"),
        text: article.to_string(),
    }];
    let pinned = PinnedCorpus::new(
        "wixqa-kb-v1",
        clean.iter().map(|object| {
            (
                object.object_id.clone(),
                object.content_sha256.clone().expect("hash"),
            )
        }),
    );

    let report = LeakageScanner::new()
        .scan(&pinned, &clean, &cases)
        .expect("a legitimate support article must pass");
    assert!(
        report
            .informational
            .iter()
            .any(|finding| matches!(finding, LeakageFinding::SourceDocumentOverlap { .. })),
        "expected the legitimate overlap to be recorded: {:?}",
        report.informational
    );

    let leak_text = "How long is the signing key rotation window? \
        The rotation window is twenty four hours";
    let mut leaked = clean.clone();
    leaked.push(CorpusObject {
        object_id: "kb-002".to_string(),
        declared_kind: ArtifactKind::SourceDocument,
        content_sha256: Some(sha256_of(leak_text)),
        provenance: provenance("https://support.example.test/kb-002"),
        text: leak_text.to_string(),
    });
    let leaked_pinned = PinnedCorpus::new(
        "wixqa-kb-v1",
        leaked.iter().map(|object| {
            (
                object.object_id.clone(),
                object.content_sha256.clone().expect("hash"),
            )
        }),
    );

    let error = LeakageScanner::new()
        .scan(&leaked_pinned, &leaked, &cases)
        .expect_err("a seeded answer-key page must fail closed");
    let ContaminationError::LeakageDetected { findings, .. } = &error else {
        panic!("expected leakage, got {error}");
    };
    assert!(
        findings.iter().any(|finding| matches!(
            finding,
            LeakageFinding::QuestionAnswerPairLeak { object_id, .. } if object_id == "kb-002"
        )),
        "findings {findings:?}"
    );
}

#[test]
fn a_missing_or_mutated_corpus_hash_fails_closed_before_any_scoring() {
    let article = "Billing invoices are issued on the first business day of each month.";
    let object = CorpusObject {
        object_id: "kb-010".to_string(),
        declared_kind: ArtifactKind::SourceDocument,
        content_sha256: Some(sha256_of(article)),
        provenance: provenance("https://support.example.test/kb-010"),
        text: article.to_string(),
    };
    let pinned = PinnedCorpus::new("wixqa-kb-v1", [("kb-010".to_string(), sha256_of(article))]);

    let mut unhashed = object.clone();
    unhashed.content_sha256 = None;
    assert!(matches!(
        LeakageScanner::new()
            .scan(&pinned, &[unhashed], &[])
            .expect_err("missing hash"),
        ContaminationError::LeakageDetected { .. }
    ));

    let mut mutated = object;
    mutated.text = format!("{article} Updated for 2027.");
    mutated.content_sha256 = Some(sha256_of(&mutated.text));
    assert!(matches!(
        LeakageScanner::new()
            .scan(&pinned, &[mutated], &[])
            .expect_err("mutated content"),
        ContaminationError::LeakageDetected { .. }
    ));
}

#[test]
fn a_paired_comparison_rejects_a_corpus_generated_from_different_seeds() {
    // Pins: the anchor cohort is the pairing key. Two memory corpora generated
    // from different seeds hold different cases and can never be compared as if
    // paired, no matter how similar their manifests look.
    let baseline_corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("baseline corpus");
    let reseeded_corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![7, 8, 9], TranscriptStyle::Marked)
            .expect("reseeded corpus");

    let anchor = |corpus: &moa_eval::memory_eval::GeneratedMemoryEvalCorpus| AnchorCohort {
        anchor_id: "memory-retrieval-anchor-2026-07".to_string(),
        manifest_hash: sha256_of(&format!(
            "{}:{:?}",
            corpus.manifest.corpus_id, corpus.manifest.seeds
        )),
        corpus_id: corpus.manifest.corpus_id.clone(),
        seeds: corpus.manifest.seeds.clone(),
        frozen_at: Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0).unwrap(),
        case_ids: corpus
            .probes
            .iter()
            .map(|probe| probe.probe_id.clone())
            .collect::<BTreeSet<_>>(),
    };

    let baseline = anchor(&baseline_corpus);
    let reseeded = anchor(&reseeded_corpus);

    require_paired(
        &PairedRunIdentity::from_anchor(&baseline),
        &PairedRunIdentity::from_anchor(&baseline),
    )
    .expect("the same anchor pairs with itself");

    let error = require_paired(
        &PairedRunIdentity::from_anchor(&baseline),
        &PairedRunIdentity::from_anchor(&reseeded),
    )
    .expect_err("different seeds must not pair");
    assert!(matches!(error, CohortError::UnpairedComparison { .. }));

    assert_eq!(
        baseline
            .ensure_unchanged(&reseeded)
            .expect_err("anchor is immutable"),
        CohortError::AnchorOverwrite {
            anchor_id: baseline.anchor_id.clone(),
            existing: baseline.manifest_hash.clone(),
            proposed: reseeded.manifest_hash.clone(),
        }
    );
}

#[test]
fn the_dataset_package_validator_trips_on_a_seeded_answer_key_or_duplicate_question() {
    use moa_eval::external_memory::dataset::{
        EvidenceLabels, ExternalMemoryCaseV1, ExternalMemorySession, ExternalMemoryTurn,
        scan_package_leakage, validate_case,
    };

    // Pins: the external-memory package validator refuses a package that leaks its
    // own answers, and accepts one whose evidence turn merely contains the answer.
    let case = |key: &str, question: &str, answer: &str, turn_text: &str| {
        validate_case(ExternalMemoryCaseV1 {
            schema_version: 1,
            isolation_key: key.to_string(),
            sessions: vec![ExternalMemorySession {
                source_id: format!("{key}-session"),
                occurred_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                turns: vec![ExternalMemoryTurn {
                    source_id: format!("{key}-turn"),
                    occurred_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 1).unwrap(),
                    role: "user".to_string(),
                    text: turn_text.to_string(),
                }],
            }],
            question: question.to_string(),
            options: Vec::new(),
            answer: answer.to_string(),
            category: "single-session-user".to_string(),
            evidence_labels: EvidenceLabels::default(),
        })
        .expect("fixture case is valid")
    };

    let legitimate = vec![
        case(
            "case-1",
            "Which city did the speaker move to for the new role?",
            "they moved to lisbon for the new role",
            "I finally accepted it, so they moved to lisbon for the new role next month.",
        ),
        case(
            "case-2",
            "What instrument did the speaker start learning?",
            "the speaker started learning the cello",
            "Signed up for lessons; the speaker started learning the cello last week.",
        ),
    ];
    scan_package_leakage(&legitimate).expect("evidence turns that carry the answer must pass");

    // The same question over *different* evidence with a different answer is how a
    // persona benchmark is built, so it must pass.
    let mut persona_variant = legitimate.clone();
    persona_variant.push(case(
        "case-3",
        "Which city did the speaker move to for the new role?",
        "they moved to porto for the new role",
        "Actually it changed: they moved to porto for the new role.",
    ));
    scan_package_leakage(&persona_variant)
        .expect("a shared question over different evidence must pass");

    let mut duplicated = legitimate.clone();
    duplicated.push(case(
        "case-1-copy",
        "Which city did the speaker move to for the new role?",
        "they moved to lisbon for the new role",
        "I finally accepted it, so they moved to lisbon for the new role next month.",
    ));
    let error = scan_package_leakage(&duplicated).expect_err("a duplicated case must fail");
    assert!(
        error
            .to_string()
            .contains("same question and the same evidence"),
        "unexpected error: {error}"
    );

    let mut answer_key = legitimate.clone();
    answer_key.push(case(
        "case-4",
        "What instrument did the speaker take up in spring?",
        "the speaker took up the oboe in spring",
        "What instrument did the speaker take up in spring? \
         the speaker took up the oboe in spring",
    ));
    let error = scan_package_leakage(&answer_key).expect_err("an in-package answer key must fail");
    assert!(
        error.to_string().contains("answer key"),
        "unexpected error: {error}"
    );
}
