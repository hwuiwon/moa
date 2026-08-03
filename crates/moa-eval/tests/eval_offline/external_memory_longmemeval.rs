//! Hermetic LongMemEval-S cleaned loader, metric, and rubric contract tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moa_eval::external_memory::answer::{AnswerScoreOutcome, AnswerScorer, ReaderResponse};
use moa_eval::external_memory::cost::{NormalizedUsage, UsageProvenance};
use moa_eval::external_memory::dataset::{DatasetPackageFormat, DatasetPackageRegistry};
use moa_eval::external_memory::longmemeval::{
    LONGMEMEVAL_ABSTENTION_COUNT, LONGMEMEVAL_EVALUATOR_COMMIT,
    LONGMEMEVAL_EVALUATOR_SOURCE_SHA256, LONGMEMEVAL_PACKAGE_SHA256, LONGMEMEVAL_QUESTION_COUNT,
    LONGMEMEVAL_RETRIEVAL_COUNT, LONGMEMEVAL_REVISION, LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON,
    LongMemEvalAnswerScorer, LongMemEvalFixtureManifest, LongMemEvalOccurrenceRef,
    LongMemEvalQuestionType, LongMemEvalRubricKind, aggregate_retrieval_metrics,
    load_longmemeval_file, load_upstream_contract, official_longmemeval_manifest,
    parse_absolute_judge_label, rubric_bundle_sha256, score_retrieval_case,
};
use serde_json::{Value, json};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external_memory/longmemeval")
}

fn tiny_dataset_path() -> PathBuf {
    fixture_root().join("longmemeval_s_cleaned_tiny.json")
}

fn load_fixture_value() -> Value {
    serde_json::from_slice(
        &std::fs::read(tiny_dataset_path()).expect("read LongMemEval tiny source fixture"),
    )
    .expect("parse LongMemEval tiny source fixture")
}

fn write_json_fixture(value: &Value, name: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("create isolated malformed fixture directory");
    let bytes = serde_json::to_vec(value).expect("serialize malformed fixture");
    std::fs::write(temp.path().join(name), bytes).expect("write malformed fixture");
    temp
}

#[test]
fn external_memory_longmemeval_loader_is_strict_and_preserves_occurrence_provenance() {
    // Pins: strict source parsing retains duplicate raw-session occurrences while sorting by
    // timestamp and mapping independent session/turn gold labels.
    let dataset = load_longmemeval_file(&tiny_dataset_path()).expect("load strict tiny package");
    assert_eq!(dataset.cases.len(), 7);
    assert_eq!(dataset.abstention_count(), 1);
    assert_eq!(dataset.retrieval_count(), 6);

    let counts = dataset.question_type_counts();
    assert_eq!(counts.len(), 6);
    assert_eq!(
        counts,
        BTreeMap::from([
            (LongMemEvalQuestionType::KnowledgeUpdate, 1),
            (LongMemEvalQuestionType::MultiSession, 1),
            (LongMemEvalQuestionType::SingleSessionAssistant, 1),
            (LongMemEvalQuestionType::SingleSessionPreference, 1),
            (LongMemEvalQuestionType::SingleSessionUser, 2),
            (LongMemEvalQuestionType::TemporalReasoning, 1),
        ])
    );

    let case = dataset
        .case("q-knowledge")
        .expect("knowledge fixture case should exist");
    assert_eq!(case.prepared.case.answer, "updated");
    assert_eq!(
        case.prepared.case.isolation_key,
        format!("longmemeval-s-cleaned/{LONGMEMEVAL_REVISION}/q-knowledge")
    );
    assert_eq!(
        case.prepared
            .case
            .sessions
            .iter()
            .map(|session| session.source_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "longmemeval/q-knowledge/session/1",
            "longmemeval/q-knowledge/session/2",
            "longmemeval/q-knowledge/session/0",
        ]
    );
    assert_eq!(
        case.session_provenance
            .iter()
            .map(|session| (
                session.raw_session_id.as_str(),
                session.original_session_index
            ))
            .collect::<Vec<_>>(),
        vec![("gold", 1), ("gold", 2), ("shared", 0)]
    );
    assert_eq!(
        case.prepared.case.evidence_labels.session_source_ids,
        Some(vec![
            "longmemeval/q-knowledge/session/1".to_string(),
            "longmemeval/q-knowledge/session/2".to_string(),
        ])
    );
    assert_eq!(
        case.prepared.case.evidence_labels.turn_source_ids,
        Some(vec!["longmemeval/q-knowledge/session/1/turn/1".to_string()])
    );
    assert_ne!(
        case.prepared.case.evidence_labels.session_source_ids,
        case.prepared.case.evidence_labels.turn_source_ids,
        "session and turn gold must remain independent"
    );

    let numeric = dataset.case("q-temporal").expect("numeric fixture case");
    assert_eq!(numeric.prepared.case.answer, "42");
    let abstention = dataset.case("q-user_abs").expect("abstention fixture case");
    assert!(abstention.is_abstention);
    assert_eq!(abstention.prepared.case.evidence_labels, Default::default());
}

#[test]
fn external_memory_longmemeval_loader_rejects_alignment_duplicates_dates_and_references() {
    // Pins: common upstream corruption modes fail at the strict source boundary with the field
    // responsible for the failure, before generic case formation.
    let base = load_fixture_value();
    let mut mutations = Vec::<(&str, Value, &str)>::new();

    let mut misaligned = base.clone();
    misaligned[0]["haystack_dates"] = json!(["2024/02/02 (Fri) 10:00"]);
    mutations.push(("misaligned.json", misaligned, "haystack arrays"));

    let mut duplicate = base.clone();
    duplicate[1]["question_id"] = duplicate[0]["question_id"].clone();
    mutations.push(("duplicate.json", duplicate, "duplicate question_id"));

    let mut bad_weekday = base.clone();
    bad_weekday[0]["question_date"] = json!("2024/02/03 (Fri) 10:00");
    mutations.push(("weekday.json", bad_weekday, "question_date"));

    let mut missing_reference = base.clone();
    missing_reference[0]["answer_session_ids"] = json!(["missing"]);
    mutations.push((
        "missing-reference.json",
        missing_reference,
        "answer_session_ids",
    ));

    let mut unknown = base;
    unknown[0]["unexpected"] = json!(true);
    mutations.push(("unknown.json", unknown, "unknown field"));

    for (name, value, expected) in mutations {
        let temp = write_json_fixture(&value, name);
        let error = load_longmemeval_file(&temp.path().join(name))
            .expect_err("malformed LongMemEval source must fail");
        assert!(
            error.to_string().contains(expected),
            "{name} should name `{expected}`, got {error}"
        );
    }
}

#[test]
fn external_memory_longmemeval_metrics_pin_ndcg_and_effective_session_cutoff() {
    // Pins: ranked turn occurrence order is authoritative, direct turn cutoffs differ from the
    // effective-k turn-to-session prefix, and duplicate ranked turns are invalid.
    let dataset = load_longmemeval_file(&tiny_dataset_path()).expect("load strict tiny package");
    let case = dataset
        .case("q-knowledge")
        .expect("knowledge fixture case should exist");
    let ranked = vec![
        LongMemEvalOccurrenceRef::new(
            "longmemeval/q-knowledge/session/0",
            "longmemeval/q-knowledge/session/0/turn/0",
        ),
        LongMemEvalOccurrenceRef::new(
            "longmemeval/q-knowledge/session/0",
            "longmemeval/q-knowledge/session/0/turn/1",
        ),
        LongMemEvalOccurrenceRef::new(
            "longmemeval/q-knowledge/session/1",
            "longmemeval/q-knowledge/session/1/turn/1",
        ),
        LongMemEvalOccurrenceRef::new(
            "longmemeval/q-knowledge/session/2",
            "longmemeval/q-knowledge/session/2/turn/0",
        ),
    ];
    let score = score_retrieval_case(case, &ranked).expect("score valid authoritative ranking");
    assert_eq!(score.turn_at_5.recall_any, 1.0);
    assert_eq!(score.turn_at_5.recall_all, 1.0);
    assert!((score.turn_at_5.ndcg - 0.630_929_753_571_457_5).abs() < 1e-12);
    assert_eq!(score.session_at_5.recall_any, 1.0);
    assert_eq!(score.session_at_5.recall_all, 1.0);
    assert!((score.session_at_5.ndcg - 0.429_859_349_926_098_6).abs() < 1e-12);
    assert_eq!(score.session_at_5.scanned_occurrences, 4);
    assert_eq!(score.session_at_5.unique_occurrences, 3);

    let duplicate = vec![ranked[0].clone(), ranked[0].clone()];
    let error = score_retrieval_case(case, &duplicate)
        .expect_err("duplicate turn occurrence IDs must be rejected");
    assert!(
        error
            .to_string()
            .contains("duplicate ranked turn occurrence")
    );
}

#[test]
fn external_memory_longmemeval_short_ranking_uses_requested_k_for_ideal_dcg() {
    // Pins: a short ranking with one of two gold occurrences remains penalized
    // against the full ideal DCG at k=5 instead of normalizing against one hit.
    let dataset = load_longmemeval_file(&tiny_dataset_path()).expect("load strict tiny package");
    let case = dataset.case("q-multi").expect("multi-session fixture case");
    let ranked = [LongMemEvalOccurrenceRef::new(
        "longmemeval/q-multi/session/0",
        "longmemeval/q-multi/session/0/turn/0",
    )];

    let score = score_retrieval_case(case, &ranked).expect("score short authoritative ranking");

    assert_eq!(score.turn_at_5.recall_any, 1.0);
    assert_eq!(score.turn_at_5.recall_all, 0.0);
    assert_eq!(score.turn_at_5.ndcg, 0.5);
    assert_eq!(score.session_at_5.recall_any, 1.0);
    assert_eq!(score.session_at_5.recall_all, 0.0);
    assert_eq!(score.session_at_5.ndcg, 0.5);
}

#[test]
fn external_memory_longmemeval_aggregate_keeps_retrieval_denominator_and_type_slices() {
    // Pins: absent rankings remain zero-valued observations rather than shrinking retrieval or
    // official-type denominators, while abstention cases never enter retrieval metrics.
    let dataset = load_longmemeval_file(&tiny_dataset_path()).expect("load strict tiny package");
    let mut rankings = BTreeMap::new();
    let preference = dataset
        .case("q-preference")
        .expect("preference fixture case");
    let gold_turn = preference
        .prepared
        .case
        .evidence_labels
        .turn_source_ids
        .as_ref()
        .expect("retrieval fixture has turn gold")[0]
        .clone();
    let gold_session = preference
        .prepared
        .case
        .evidence_labels
        .session_source_ids
        .as_ref()
        .expect("retrieval fixture has session gold")[0]
        .clone();
    rankings.insert(
        "q-preference".to_string(),
        vec![LongMemEvalOccurrenceRef::new(gold_session, gold_turn)],
    );

    let report = aggregate_retrieval_metrics(&dataset.cases, &rankings)
        .expect("aggregate complete-denominator retrieval report");
    assert_eq!(report.denominator, 6);
    assert_eq!(report.session_at_5.recall_any.denominator, 6);
    assert_eq!(report.session_at_5.recall_any.numerator, 1.0);
    assert_eq!(report.turn_at_50.recall_all.denominator, 6);
    assert_eq!(report.question_type_slices.len(), 6);
    assert_eq!(
        report.question_type_slices[&LongMemEvalQuestionType::SingleSessionPreference]
            .turn_at_5
            .recall_any
            .value,
        1.0
    );
    assert_eq!(
        report.question_type_slices[&LongMemEvalQuestionType::SingleSessionUser]
            .turn_at_5
            .recall_any
            .value,
        0.0
    );
}

#[test]
fn external_memory_longmemeval_rubrics_pin_upstream_bytes_mapping_and_strict_parser() {
    // Pins: evaluator prompts are vendored byte-for-byte, mapped by official category, and judge
    // parsing deliberately hardens upstream substring matching to exact yes/no labels.
    let contract = load_upstream_contract(&fixture_root().join("upstream_contract_v1.json"))
        .expect("validate committed upstream evaluator contract");
    assert_eq!(contract.evaluator_commit, LONGMEMEVAL_EVALUATOR_COMMIT);
    assert_eq!(
        contract.evaluator_source_sha256,
        LONGMEMEVAL_EVALUATOR_SOURCE_SHA256
    );
    assert_eq!(
        rubric_bundle_sha256().expect("hash rubric bundle"),
        contract.bundle_sha256
    );
    for kind in LongMemEvalRubricKind::ALL {
        assert_eq!(
            kind.computed_sha256(),
            contract.rubrics[&kind].sha256,
            "rubric hash mismatch for {kind}"
        );
    }

    assert_eq!(
        LongMemEvalRubricKind::for_question(LongMemEvalQuestionType::MultiSession, false),
        LongMemEvalRubricKind::General
    );
    assert_eq!(
        LongMemEvalRubricKind::for_question(LongMemEvalQuestionType::TemporalReasoning, false),
        LongMemEvalRubricKind::TemporalReasoning
    );
    assert_eq!(
        LongMemEvalRubricKind::for_question(LongMemEvalQuestionType::KnowledgeUpdate, true),
        LongMemEvalRubricKind::Abstention
    );
    assert_eq!(parse_absolute_judge_label(" YES\n"), Some(true));
    assert_eq!(parse_absolute_judge_label("no"), Some(false));
    assert_eq!(parse_absolute_judge_label("yes, because"), None);
    assert_eq!(parse_absolute_judge_label("not yes"), None);
    assert_eq!(parse_absolute_judge_label(""), None);

    let dataset = load_longmemeval_file(&tiny_dataset_path()).expect("load scorer fixture");
    let outcome = LongMemEvalAnswerScorer
        .score(
            &dataset.cases[0].prepared.case,
            &ReaderResponse {
                answer: "candidate".to_string(),
                model: "fixture-reader".to_string(),
                prompt_version: "reader-v1".to_string(),
                usage: NormalizedUsage {
                    input_tokens_uncached: 0,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 0,
                    provenance: UsageProvenance::Actual,
                },
                latency_ms: 0,
            },
        )
        .expect("LongMemEval unsupported scorer is not an error");
    assert_eq!(
        outcome,
        AnswerScoreOutcome::Unsupported {
            reason: LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON.to_string(),
        }
    );
}

#[test]
fn external_memory_longmemeval_fixture_and_official_provenance_are_self_consistent() {
    // Pins: hermetic fixture bytes carry explicit synthetic provenance while the production
    // manifest keeps the immutable official file and package identities.
    let bytes = std::fs::read(fixture_root().join("fixture_manifest.json"))
        .expect("read LongMemEval fixture manifest");
    let manifest: LongMemEvalFixtureManifest =
        serde_json::from_slice(&bytes).expect("parse strict fixture manifest");
    manifest
        .validate(&fixture_root())
        .expect("fixture manifest validates bytes, IDs, and counts");

    let official = official_longmemeval_manifest();
    assert_eq!(official.source.revision, LONGMEMEVAL_REVISION);
    assert_eq!(official.files.len(), 1);
    assert_eq!(
        official
            .canonical_hash()
            .expect("canonicalize official LongMemEval manifest"),
        LONGMEMEVAL_PACKAGE_SHA256
    );
    assert_eq!(LONGMEMEVAL_QUESTION_COUNT, 500);
    assert_eq!(LONGMEMEVAL_ABSTENTION_COUNT, 30);
    assert_eq!(LONGMEMEVAL_RETRIEVAL_COUNT, 470);
    assert_eq!(
        DatasetPackageRegistry::task_10()
            .entry(moa_eval::external_memory::longmemeval::LONGMEMEVAL_DATASET)
            .expect("LongMemEval registry entry")
            .format,
        DatasetPackageFormat::LongMemEvalSCleaned
    );
}
