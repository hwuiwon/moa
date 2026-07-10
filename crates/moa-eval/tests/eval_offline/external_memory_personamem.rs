use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moa_eval::external_memory::answer::{
    AnswerScoreOutcome, AnswerScorer, ReaderResponse, SupportStatus,
};
use moa_eval::external_memory::cost::{NormalizedUsage, UsageProvenance};
use moa_eval::external_memory::dataset::{
    DatasetFileProvenance, DatasetPackageManifestV1, DatasetPackageSourceV1, DatasetPackageV1,
};
use moa_eval::external_memory::personamem::{
    PERSONAMEM_DATASET, PERSONAMEM_QUESTIONS_SHA256, PERSONAMEM_REPOSITORY, PERSONAMEM_REVISION,
    PERSONAMEM_SHARED_CONTEXTS_SHA256, PersonaMemAnswerOutcome, PersonaMemFixtureManifestV1,
    PersonaMemLabelScorerV1, build_accuracy_report, load_personamem_files,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external_memory/personamem")
}

fn reader(answer: &str) -> ReaderResponse {
    ReaderResponse {
        answer: answer.to_string(),
        model: "fixture-reader".to_string(),
        prompt_version: "personamem-label-v1".to_string(),
        usage: NormalizedUsage {
            input_tokens_uncached: 0,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            provenance: UsageProvenance::Actual,
        },
        latency_ms: 0,
    }
}

fn score_value(
    scorer: &impl AnswerScorer,
    case: &moa_eval::external_memory::dataset::ExternalMemoryCaseV1,
    answer: &str,
) -> f64 {
    match scorer.score(case, &reader(answer)).expect("score") {
        AnswerScoreOutcome::Supported(score) => score.value,
        AnswerScoreOutcome::Unsupported { reason } => {
            panic!("PersonaMem scoring unexpectedly unsupported: {reason}")
        }
    }
}

#[test]
fn external_memory_personamem_manifest_hash_is_canonical_and_every_field_sensitive() {
    // Pins: package.json hashes only the canonical inner manifest with domain separation.
    let manifest = DatasetPackageManifestV1 {
        schema_version: 1,
        dataset: PERSONAMEM_DATASET.to_string(),
        source: DatasetPackageSourceV1 {
            repository: PERSONAMEM_REPOSITORY.to_string(),
            revision: PERSONAMEM_REVISION.to_string(),
        },
        files: vec![
            DatasetFileProvenance {
                path: "questions_32k.csv".to_string(),
                size_bytes: 1_305_366,
                sha256: PERSONAMEM_QUESTIONS_SHA256.to_string(),
            },
            DatasetFileProvenance {
                path: "shared_contexts_32k.jsonl".to_string(),
                size_bytes: 5_613_210,
                sha256: PERSONAMEM_SHARED_CONTEXTS_SHA256.to_string(),
            },
        ],
    };
    let package = DatasetPackageV1::new(manifest.clone()).expect("official manifest should hash");
    assert_eq!(
        package.package_sha256,
        "f4baf9ffa83a8452b5a026564eb439caa94334020d49be84510d392a88fe94ac"
    );

    let mut reordered = serde_json::Map::new();
    let value = serde_json::to_value(&manifest).expect("serialize manifest");
    for (key, value) in value.as_object().expect("manifest object").iter().rev() {
        reordered.insert(key.clone(), value.clone());
    }
    assert_eq!(
        package.package_sha256,
        DatasetPackageManifestV1::canonical_hash_value(&serde_json::Value::Object(reordered))
            .expect("reordered manifest should hash")
    );

    for pointer in [
        "/schema_version",
        "/dataset",
        "/source/repository",
        "/source/revision",
        "/files/0/path",
        "/files/0/size_bytes",
        "/files/0/sha256",
    ] {
        let mut changed = serde_json::to_value(&manifest).expect("serialize manifest");
        let leaf = changed.pointer_mut(pointer).expect("manifest pointer");
        *leaf = match leaf {
            serde_json::Value::Number(number) => {
                serde_json::json!(number.as_u64().expect("u64") + 1)
            }
            serde_json::Value::String(value) if pointer.ends_with("sha256") => {
                let replacement = if value.starts_with('a') { 'b' } else { 'a' };
                let suffix = value.chars().skip(1).collect::<String>();
                serde_json::Value::String(format!("{replacement}{suffix}"))
            }
            serde_json::Value::String(value) => {
                serde_json::Value::String(format!("{value}-changed"))
            }
            other => panic!("unexpected leaf {other:?}"),
        };
        assert_ne!(
            package.package_sha256,
            DatasetPackageManifestV1::canonical_hash_value(&changed)
                .expect("changed manifest should hash"),
            "hash omitted {pointer}"
        );
    }
}

#[test]
fn external_memory_personamem_package_json_is_strict_and_files_are_sorted() {
    // Pins: package.json has exactly the wrapper fields and package.json never self-hashes.
    let root = fixture_root();
    let package = tiny_package(&root);
    let encoded = serde_json::to_vec(&package).expect("serialize package");
    let decoded: DatasetPackageV1 = serde_json::from_slice(&encoded).expect("strict package");
    decoded.validate().expect("package should validate");

    let mut unknown = serde_json::to_value(&package).expect("serialize package");
    unknown
        .as_object_mut()
        .expect("package object")
        .insert("extra".to_string(), serde_json::json!(true));
    assert!(serde_json::from_value::<DatasetPackageV1>(unknown).is_err());

    let mut nested_unknown = serde_json::to_value(&package).expect("serialize package");
    nested_unknown["manifest"]
        .as_object_mut()
        .expect("manifest object")
        .insert("extra".to_string(), serde_json::json!(true));
    assert!(serde_json::from_value::<DatasetPackageV1>(nested_unknown).is_err());

    let mut unsorted = package.clone();
    unsorted.manifest.files.reverse();
    assert!(unsorted.validate().is_err());
    let mut self_referential = package;
    self_referential.manifest.files[0].path = "package.json".to_string();
    assert!(self_referential.validate().is_err());

    let registry = moa_eval::external_memory::dataset::DatasetPackageRegistry::task_9();
    assert!(registry.entry(PERSONAMEM_DATASET).is_some());
}

#[test]
fn external_memory_personamem_loader_is_strict_and_preserves_typed_metadata() {
    // Pins: exact 15-column CSV, Python/JSON option literals, typed metadata, and context joins.
    let root = fixture_root();
    let dataset = load_personamem_files(
        &root.join("questions_32k_tiny.csv"),
        &root.join("shared_contexts_32k_tiny.jsonl"),
    )
    .expect("tiny PersonaMem fixture should load");
    assert_eq!(dataset.cases.len(), 3);
    assert_eq!(dataset.persona_count(), 2);
    assert_eq!(dataset.context_count, 2);
    assert_eq!(dataset.cases[0].options[0].label, "(a)");
    assert_eq!(dataset.cases[0].options[1].text, "blue");
    assert_eq!(dataset.cases[1].options[2].text, "curry");
    assert_eq!(dataset.cases[0].metadata.persona_id, 0);
    assert_eq!(dataset.cases[0].metadata.distance_to_ref_in_blocks, 2);
    assert_eq!(
        dataset.cases[0]
            .metadata
            .distance_to_ref_proportion_in_context,
        "50.00%"
    );
    assert_eq!(
        dataset.cases[0].prepared.case.question,
        "Which color did I prefer?"
    );
    assert_eq!(dataset.cases[0].prepared.case.answer, "(b)");
    assert!(
        dataset.cases[0]
            .prepared
            .case
            .evidence_labels
            .turn_source_ids
            .is_none()
    );
}

#[test]
fn external_memory_personamem_csv_rejects_schema_identifier_numeric_and_option_drift() {
    // Pins: malformed headers, IDs, numeric buckets, options, gold labels, joins, and slices fail closed.
    let root = fixture_root();
    let questions = std::fs::read_to_string(root.join("questions_32k_tiny.csv"))
        .expect("read question fixture");
    let contexts = std::fs::read_to_string(root.join("shared_contexts_32k_tiny.jsonl"))
        .expect("read context fixture");
    let mutations = [
        questions.replacen("persona_id,question_id", "question_id,persona_id", 1),
        questions.replacen("fixture-question-beta", "fixture-question-alpha", 1),
        questions.replacen(",2,64,0,50.00%,", ",8,64,0,50.00%,", 1),
        questions.replacen("(b),\"['(a) green'", "(z),\"['(a) green'", 1),
        questions.replacen("'(d) amber'", "'(c) red'", 1),
        questions.replacen("fixture-context-beta,4", "missing-context,4", 1),
        questions.replacen("fixture-context-alpha,9", "fixture-context-alpha,99", 1),
        questions.replacen("50.00%", "not-a-percentage", 1),
    ];
    for (index, mutation) in mutations.into_iter().enumerate() {
        let temp = TempDir::new().expect("tempdir");
        let question_path = temp.path().join("questions.csv");
        let context_path = temp.path().join("contexts.jsonl");
        std::fs::write(&question_path, mutation).expect("write mutated questions");
        std::fs::write(&context_path, &contexts).expect("write contexts");
        assert!(
            load_personamem_files(&question_path, &context_path).is_err(),
            "mutation {index} must fail"
        );
    }
}

#[test]
fn external_memory_personamem_contexts_reject_non_singleton_duplicate_and_invalid_messages() {
    // Pins: context JSONL is nonblank one-key records with unique IDs and exact role/content messages.
    let root = fixture_root();
    let questions =
        std::fs::read(root.join("questions_32k_tiny.csv")).expect("read question fixture");
    let valid_contexts = std::fs::read_to_string(root.join("shared_contexts_32k_tiny.jsonl"))
        .expect("read context fixture");
    let first_line = valid_contexts.lines().next().expect("first context line");
    let mutations = [
        format!("\n{valid_contexts}"),
        "{}\n".to_string(),
        format!("{first_line}\n{first_line}\n"),
        valid_contexts.replacen("\"role\":\"assistant\"", "\"role\":\"tool\"", 1),
        valid_contexts.replacen(
            "\"content\":\"Leading assistant one.\"",
            "\"content\":\"Leading assistant one.\",\"extra\":true",
            1,
        ),
        valid_contexts.replacen("Leading assistant one.", "", 1),
    ];
    for (index, mutation) in mutations.into_iter().enumerate() {
        let temp = TempDir::new().expect("tempdir");
        let question_path = temp.path().join("questions.csv");
        let context_path = temp.path().join("contexts.jsonl");
        std::fs::write(&question_path, &questions).expect("write questions");
        std::fs::write(&context_path, mutation).expect("write mutated contexts");
        assert!(
            load_personamem_files(&question_path, &context_path).is_err(),
            "context mutation {index} must fail"
        );
    }
}

#[test]
fn external_memory_personamem_projection_is_end_exclusive_and_lossless() {
    // Pins: systems split sessions, occurrence timestamps follow source indices, and logical turns retain partial runs.
    let root = fixture_root();
    let dataset = load_personamem_files(
        &root.join("questions_32k_tiny.csv"),
        &root.join("shared_contexts_32k_tiny.jsonl"),
    )
    .expect("tiny PersonaMem fixture should load");
    let full = &dataset.cases[0];
    assert_eq!(full.history.sessions.len(), 3);
    assert_eq!(
        full.history.sessions[0]
            .occurrences
            .iter()
            .map(|occurrence| (
                occurrence.original_index,
                occurrence.logical_turn_index,
                occurrence.role.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![(0, 0, "assistant"), (1, 0, "assistant")]
    );
    assert_eq!(
        full.history.sessions[1]
            .occurrences
            .iter()
            .map(|occurrence| (
                occurrence.original_index,
                occurrence.logical_turn_index,
                occurrence.role.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (3, 0, "user"),
            (4, 1, "user"),
            (5, 1, "assistant"),
            (6, 1, "assistant")
        ]
    );
    assert_eq!(
        full.history.sessions[2]
            .occurrences
            .iter()
            .map(|occurrence| (
                occurrence.original_index,
                occurrence.logical_turn_index,
                occurrence.role.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![(8, 0, "user")]
    );
    let occurrences = full
        .history
        .sessions
        .iter()
        .flat_map(|session| &session.occurrences)
        .collect::<Vec<_>>();
    assert!(
        occurrences
            .windows(2)
            .all(|pair| pair[0].occurred_at < pair[1].occurred_at)
    );
    assert_eq!(
        occurrences.last().expect("trailing occurrence").content,
        "I may travel tomorrow."
    );

    let sliced = &dataset.cases[2];
    assert_eq!(sliced.metadata.end_index_in_shared_context, 5);
    assert_eq!(
        sliced
            .history
            .sessions
            .iter()
            .map(|session| session.occurrences.len())
            .sum::<usize>(),
        4
    );
    assert!(
        sliced
            .history
            .sessions
            .iter()
            .flat_map(|session| &session.occurrences)
            .all(|occurrence| occurrence.original_index < 5)
    );
}

#[test]
fn external_memory_personamem_isolation_uses_revision_and_question_not_persona() {
    // Pins: questions sharing persona/context never share a backend isolation boundary.
    let root = fixture_root();
    let dataset = load_personamem_files(
        &root.join("questions_32k_tiny.csv"),
        &root.join("shared_contexts_32k_tiny.jsonl"),
    )
    .expect("tiny PersonaMem fixture should load");
    let first = &dataset.cases[0];
    let same_persona = &dataset.cases[2];
    assert_eq!(first.metadata.persona_id, same_persona.metadata.persona_id);
    assert_ne!(
        first.prepared.case.isolation_key,
        same_persona.prepared.case.isolation_key
    );
    assert!(
        first
            .prepared
            .case
            .isolation_key
            .contains(PERSONAMEM_REVISION)
    );
    assert!(
        first
            .prepared
            .case
            .isolation_key
            .contains(&first.metadata.question_id)
    );
    assert!(!first.prepared.case.isolation_key.ends_with("/0"));
}

#[test]
fn external_memory_personamem_label_scorer_rejects_ambiguity_and_keeps_denominator() {
    // Pins: distinct extracted labels must equal only the gold label; every outcome contributes one.
    let root = fixture_root();
    let dataset = load_personamem_files(
        &root.join("questions_32k_tiny.csv"),
        &root.join("shared_contexts_32k_tiny.jsonl"),
    )
    .expect("tiny PersonaMem fixture should load");
    let scorer = PersonaMemLabelScorerV1;
    let case = &dataset.cases[0].prepared.case;
    assert_eq!(score_value(&scorer, case, "Answer: (b)"), 1.0);
    assert_eq!(score_value(&scorer, case, "(b), confirmed: (b)"), 1.0);
    assert_eq!(score_value(&scorer, case, "(b), but perhaps (a)"), 0.0);
    assert_eq!(score_value(&scorer, case, "blue"), 0.0);

    let outcomes = BTreeMap::from([
        (
            "fixture-question-alpha".to_string(),
            PersonaMemAnswerOutcome::Answer("(b)".to_string()),
        ),
        (
            "fixture-question-beta".to_string(),
            PersonaMemAnswerOutcome::ProviderFailure,
        ),
        (
            "fixture-question-alpha-slice".to_string(),
            PersonaMemAnswerOutcome::Answer("(a) and (d)".to_string()),
        ),
    ]);
    let report = build_accuracy_report(&dataset.cases, &outcomes).expect("accuracy report");
    assert_eq!(report.numerator, 1);
    assert_eq!(report.denominator, 3);
    assert_eq!(report.cluster_count, 2);
    assert_eq!(report.bootstrap.resamples, 10_000);
    assert_eq!(report.bootstrap.seed, 0x7a2b_3c4d_5e6f_1021);
    assert_eq!(
        report.question_type_slices["recall_user_shared_facts"].numerator,
        1
    );
    assert_eq!(report.distance_slices[&2].denominator, 1);
    assert_eq!(
        report.retrieval_recall,
        SupportStatus::Unsupported {
            reason: "PersonaMem v1 has no reliable evidence-reference labels".to_string(),
        }
    );
}

#[test]
fn external_memory_personamem_fixture_manifest_proves_provenance_and_hashes() {
    // Pins: tiny data is explicitly synthetic and its selected IDs, rationale, counts, and bytes are audited.
    let root = fixture_root();
    let fixture: PersonaMemFixtureManifestV1 = serde_json::from_slice(
        &std::fs::read(root.join("fixture_manifest.json")).expect("read fixture manifest"),
    )
    .expect("strict fixture manifest");
    fixture
        .validate(&root)
        .expect("fixture provenance should validate");
    assert_eq!(fixture.source.repository, PERSONAMEM_REPOSITORY);
    assert_eq!(fixture.source.revision, PERSONAMEM_REVISION);
    assert_eq!(fixture.source_files[0].sha256, PERSONAMEM_QUESTIONS_SHA256);
    assert_eq!(
        fixture.source_files[1].sha256,
        PERSONAMEM_SHARED_CONTEXTS_SHA256
    );
    assert_eq!(fixture.counts.questions, 3);
    assert_eq!(fixture.counts.personas, 2);
    assert_eq!(fixture.counts.contexts, 2);
    assert_eq!(fixture.content_origin, "synthetic_contract_fixture");
    assert!(
        fixture
            .selection_rationale
            .contains("projection edge cases")
    );
}

fn tiny_package(root: &Path) -> DatasetPackageV1 {
    let files = ["questions_32k_tiny.csv", "shared_contexts_32k_tiny.jsonl"]
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(root.join(path)).expect("read fixture file");
            DatasetFileProvenance {
                path: path.to_string(),
                size_bytes: u64::try_from(bytes.len()).expect("fixture length fits u64"),
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }
        })
        .collect();
    DatasetPackageV1::new(DatasetPackageManifestV1 {
        schema_version: 1,
        dataset: "personamem-32k-tiny".to_string(),
        source: DatasetPackageSourceV1 {
            repository: PERSONAMEM_REPOSITORY.to_string(),
            revision: PERSONAMEM_REVISION.to_string(),
        },
        files,
    })
    .expect("tiny package should hash")
}
