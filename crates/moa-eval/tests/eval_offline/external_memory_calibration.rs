//! Hermetic external-memory judge calibration contract tests.

use std::collections::BTreeMap;

use moa_eval::external_memory::calibration::{
    CALIBRATION_SAMPLE_SIZE, CalibrationAdjudication, CalibrationAdjudicationItem,
    CalibrationArtifactStatus, CalibrationLabel, CalibrationLabelArtifact, CalibrationManifest,
    CalibrationResults, CalibrationRole, CalibrationSourceCase, CalibrationStratum,
    CalibrationVerdict, KappaStatus, hash_identity, prepare_calibration, score_calibration,
};

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/external_memory/calibration_labels_tiny.json")
}

fn source_cases() -> Vec<CalibrationSourceCase> {
    CalibrationStratum::ALL
        .into_iter()
        .flat_map(|stratum| {
            (0..11).map(move |index| CalibrationSourceCase {
                question_id: format!("{}-{index:02}", stratum.as_str()),
                stratum,
                question: format!("question {index} for {stratum}"),
                reference_answer: format!("reference {index}"),
                candidate_answer: Some(format!("candidate {index}")),
                reader_failure_kind: None,
                judge_outcome: Some(index % 2 == 0),
            })
        })
        .collect()
}

fn completed_labels(
    template: &moa_eval::external_memory::calibration::CalibrationLabelArtifact,
    identity: &str,
    invert: bool,
) -> moa_eval::external_memory::calibration::CalibrationLabelArtifact {
    let mut artifact = template.clone();
    artifact.status = CalibrationArtifactStatus::Completed;
    artifact.identity_sha256 = Some(hash_identity(identity).expect("hash test identity"));
    for (index, item) in artifact.items.iter_mut().enumerate() {
        let correct = index % 2 == 0;
        item.label = Some(if correct ^ invert {
            CalibrationLabel::Correct
        } else {
            CalibrationLabel::Incorrect
        });
    }
    artifact
}

fn adjudication(manifest: &CalibrationManifest, identity: &str) -> CalibrationAdjudication {
    CalibrationAdjudication {
        schema_version: 1,
        manifest_sha256: manifest.manifest_sha256.clone(),
        role: CalibrationRole::Adjudicator,
        identity_sha256: hash_identity(identity).expect("hash adjudicator identity"),
        labels: manifest
            .sample
            .iter()
            .enumerate()
            .map(|(index, sample)| CalibrationAdjudicationItem {
                question_id: sample.question_id.clone(),
                label: if index % 2 == 0 {
                    CalibrationLabel::Correct
                } else {
                    CalibrationLabel::Incorrect
                },
            })
            .collect(),
    }
}

#[test]
fn external_memory_calibration_selects_exact_order_and_emits_blinded_templates() {
    // Pins: every official stratum contributes exactly ten digest-sorted cases in the specified
    // order, and labeler templates contain no identity, labels, or judge decisions.
    let cases = source_cases();
    let prepared = prepare_calibration(
        "revision-1",
        &cases,
        br#"{"package":"bytes"}"#,
        br#"{"report":"bytes"}"#,
    )
    .expect("prepare deterministic calibration sample");

    assert_eq!(prepared.manifest.sample.len(), CALIBRATION_SAMPLE_SIZE);
    assert_eq!(prepared.labeler_a.role, CalibrationRole::LabelerA);
    assert_eq!(prepared.labeler_b.role, CalibrationRole::LabelerB);
    assert_eq!(
        prepared.labeler_a.status,
        CalibrationArtifactStatus::Template
    );
    assert!(prepared.labeler_a.identity_sha256.is_none());
    assert!(
        prepared
            .labeler_a
            .items
            .iter()
            .all(|item| item.label.is_none())
    );
    assert!(
        prepared
            .labeler_a
            .items
            .iter()
            .all(|item| !item.question.contains("judge"))
    );
    assert_eq!(prepared.labeler_a.items, prepared.labeler_b.items);
    assert_eq!(
        prepared.manifest.manifest_sha256,
        "378c820dbc0bdf39d13323f4674ca6e7e7812288fcdef835d4dd5a6fe7e5a206"
    );
    assert_eq!(
        prepared.manifest.package_sha256,
        "724609fb894fd52f501ae1d22f80d4d5780411e2a672b284f31d1d4c6a4ed4d1"
    );
    assert_eq!(
        prepared.manifest.report_sha256,
        "53912639cf9046a4204be9a6b29cd93c986cce37437b63ca9ea0ec4bcd995015"
    );

    let expected_by_stratum = [
        [5, 8, 10, 4, 9, 2, 6, 7, 0, 3],
        [10, 5, 8, 3, 2, 4, 7, 9, 0, 1],
        [1, 10, 4, 5, 0, 7, 9, 2, 3, 8],
        [8, 6, 1, 7, 2, 0, 3, 4, 5, 9],
        [0, 4, 5, 3, 7, 10, 8, 1, 9, 2],
        [6, 9, 3, 8, 1, 5, 0, 7, 2, 4],
        [3, 8, 0, 1, 6, 4, 9, 5, 7, 10],
    ];
    for (stratum_index, stratum) in CalibrationStratum::ALL.into_iter().enumerate() {
        let sample = &prepared.manifest.sample[stratum_index * 10..(stratum_index + 1) * 10];
        assert!(sample.iter().all(|item| item.stratum == stratum));
        let selected_ids = sample
            .iter()
            .map(|item| item.question_id.as_str())
            .collect::<Vec<_>>();
        let expected_ids = expected_by_stratum[stratum_index]
            .into_iter()
            .map(|index| format!("{}-{index:02}", stratum.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(selected_ids, expected_ids);
    }

    prepared
        .manifest
        .validate()
        .expect("prepared manifest validates its canonical self-hash");
}

#[test]
fn external_memory_calibration_scores_confusion_kappa_accuracy_and_exact_byte_hashes() {
    // Pins: scoring retains all 70 pairs, applies total confusion/kappa/accuracy thresholds, and
    // hashes the exact input bytes rather than parsed JSON values.
    let cases = source_cases();
    let report_bytes = br#"{"report":"bytes"}"#;
    let prepared = prepare_calibration(
        "revision-1",
        &cases,
        br#"{"package":"bytes"}"#,
        report_bytes,
    )
    .expect("prepare deterministic calibration sample");
    let labeler_a = completed_labels(&prepared.labeler_a, "Alice", false);
    let labeler_b = completed_labels(&prepared.labeler_b, "Bob", false);
    let adjudication = adjudication(&prepared.manifest, "Carol");
    let manifest_bytes = serde_json::to_vec(&prepared.manifest).expect("serialize manifest");
    let labeler_a_bytes = serde_json::to_vec(&labeler_a).expect("serialize labeler A");
    let labeler_b_bytes = serde_json::to_vec(&labeler_b).expect("serialize labeler B");
    let adjudication_bytes = serde_json::to_vec(&adjudication).expect("serialize adjudication");
    let judge_outcomes = prepared
        .manifest
        .sample
        .iter()
        .enumerate()
        .map(|(index, sample)| (sample.question_id.clone(), Some(index % 2 == 0)))
        .collect::<BTreeMap<_, _>>();

    let results = score_calibration(
        &manifest_bytes,
        report_bytes,
        &labeler_a_bytes,
        &labeler_b_bytes,
        &adjudication_bytes,
        &judge_outcomes,
    )
    .expect("score completed calibration artifacts");

    assert_eq!(
        (results.n00, results.n01, results.n10, results.n11),
        (35, 0, 0, 35)
    );
    assert_eq!(results.pair_denominator, 70);
    assert_eq!(results.agreement, 1.0);
    assert_eq!(results.kappa_status, KappaStatus::Defined);
    assert_eq!(results.kappa, Some(1.0));
    assert_eq!(results.judge_correct_count, 70);
    assert_eq!(results.judge_denominator, 70);
    assert_eq!(results.judge_accuracy, 1.0);
    assert!(results.agreement_pass && results.kappa_pass && results.accuracy_pass);
    results.validate().expect("results self-hash validates");

    let mut changed_labeler_bytes = labeler_a_bytes.clone();
    changed_labeler_bytes.push(b'\n');
    let changed = score_calibration(
        &manifest_bytes,
        report_bytes,
        &changed_labeler_bytes,
        &labeler_b_bytes,
        &adjudication_bytes,
        &judge_outcomes,
    )
    .expect("trailing JSON whitespace remains parseable");
    assert_ne!(results.labeler_a_sha256, changed.labeler_a_sha256);
}

#[test]
fn external_memory_calibration_rejects_identity_content_and_schema_violations() {
    // Pins: missing labels, duplicate identities, content drift, unknown JSON fields, and a
    // manifest/report byte mismatch fail closed before metrics are accepted.
    let cases = source_cases();
    let report_bytes = br#"{"report":"bytes"}"#;
    let prepared = prepare_calibration(
        "revision-1",
        &cases,
        br#"{"package":"bytes"}"#,
        report_bytes,
    )
    .expect("prepare deterministic calibration sample");
    let labeler_a = completed_labels(&prepared.labeler_a, "same", false);
    let labeler_b = completed_labels(&prepared.labeler_b, "same", false);
    let adjudication = adjudication(&prepared.manifest, "adjudicator");
    let manifest_bytes = serde_json::to_vec(&prepared.manifest).expect("serialize manifest");
    let labeler_a_bytes = serde_json::to_vec(&labeler_a).expect("serialize labeler A");
    let labeler_b_bytes = serde_json::to_vec(&labeler_b).expect("serialize labeler B");
    let adjudication_bytes = serde_json::to_vec(&adjudication).expect("serialize adjudication");

    let error = score_calibration(
        &manifest_bytes,
        report_bytes,
        &labeler_a_bytes,
        &labeler_b_bytes,
        &adjudication_bytes,
        &BTreeMap::new(),
    )
    .expect_err("duplicate identities must fail");
    assert!(error.to_string().contains("identity"));

    let mut drifted_labeler_b = completed_labels(&prepared.labeler_b, "different", false);
    drifted_labeler_b.items[0].question.push_str(" changed");
    let drifted_labeler_b_bytes =
        serde_json::to_vec(&drifted_labeler_b).expect("serialize drifted labeler B");
    let error = score_calibration(
        &manifest_bytes,
        report_bytes,
        &labeler_a_bytes,
        &drifted_labeler_b_bytes,
        &adjudication_bytes,
        &BTreeMap::new(),
    )
    .expect_err("labeler content drift must fail");
    assert!(error.to_string().contains("sample/content equality"));

    let mut unknown: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("parse manifest value");
    unknown["unexpected"] = serde_json::json!(true);
    let error = serde_json::from_value::<CalibrationManifest>(unknown)
        .expect_err("unknown manifest fields must fail");
    assert!(error.to_string().contains("unknown field"));

    let error = score_calibration(
        &manifest_bytes,
        br#"{"different":"report"}"#,
        &labeler_a_bytes,
        &labeler_b_bytes,
        &adjudication_bytes,
        &BTreeMap::new(),
    )
    .expect_err("report byte hash must match the manifest");
    assert!(error.to_string().contains("report SHA-256"));
}

#[test]
fn external_memory_calibration_marks_zero_kappa_denominator_undefined_and_missing_judges_wrong() {
    // Pins: unanimous pre-adjudication marginals never become NaN/pass, and missing judge
    // outcomes contribute no accuracy credit even when the adjudicated label is incorrect.
    let cases = source_cases();
    let report_bytes = br#"{"report":"bytes"}"#;
    let prepared = prepare_calibration(
        "revision-1",
        &cases,
        br#"{"package":"bytes"}"#,
        report_bytes,
    )
    .expect("prepare deterministic calibration sample");
    let mut labeler_a = completed_labels(&prepared.labeler_a, "Alice", false);
    let mut labeler_b = completed_labels(&prepared.labeler_b, "Bob", false);
    for item in &mut labeler_a.items {
        item.label = Some(CalibrationLabel::Correct);
    }
    for item in &mut labeler_b.items {
        item.label = Some(CalibrationLabel::Correct);
    }
    let adjudication = adjudication(&prepared.manifest, "Carol");
    let manifest_bytes = serde_json::to_vec(&prepared.manifest).expect("serialize manifest");
    let labeler_a_bytes = serde_json::to_vec(&labeler_a).expect("serialize labeler A");
    let labeler_b_bytes = serde_json::to_vec(&labeler_b).expect("serialize labeler B");
    let adjudication_bytes = serde_json::to_vec(&adjudication).expect("serialize adjudication");

    let results = score_calibration(
        &manifest_bytes,
        report_bytes,
        &labeler_a_bytes,
        &labeler_b_bytes,
        &adjudication_bytes,
        &BTreeMap::new(),
    )
    .expect("undefined kappa is a retained failed calibration result");

    assert_eq!(results.kappa_status, KappaStatus::UndefinedZeroDenominator);
    assert_eq!(results.kappa, None);
    assert!(!results.kappa_pass);
    assert_eq!(results.judge_correct_count, 0);
    assert_eq!(results.judge_accuracy, 0.0);
    assert_eq!(results.verdict.as_str(), "fail");
}

#[test]
fn external_memory_calibration_identity_hash_uses_trimmed_nfc_text() {
    // Pins: canonically equivalent human identities hash identically after trim and NFC, while
    // blank identities are rejected and raw identity strings never enter artifacts.
    let composed = hash_identity("  Jos\u{e9}  ").expect("hash composed identity");
    let decomposed = hash_identity("Jose\u{301}").expect("hash decomposed identity");
    assert_eq!(composed, decomposed);
    assert_eq!(
        composed,
        "73a38d7cece55044ab79b55121012b5b80c2fec0c10abd4d04b14cb0bf0d9f7d"
    );
    assert_eq!(composed.len(), 64);
    assert!(
        composed
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert!(hash_identity("  ").is_err());
}

#[test]
fn external_memory_calibration_label_fixture_pins_strict_v1_wire() {
    // Pins: committed calibration labels use the strict V1 names and complete 70-item shape.
    let bytes = std::fs::read(fixture_path()).expect("read calibration label fixture");
    let artifact: CalibrationLabelArtifact =
        serde_json::from_slice(&bytes).expect("parse strict calibration label fixture");
    assert_eq!(artifact.schema_version, 1);
    assert_eq!(artifact.role, CalibrationRole::LabelerA);
    assert_eq!(artifact.status, CalibrationArtifactStatus::Completed);
    assert_eq!(artifact.items.len(), 70);
    assert_eq!(
        artifact.items[0].stratum,
        CalibrationStratum::KnowledgeUpdate
    );
    assert_eq!(artifact.items[69].stratum, CalibrationStratum::Abstention);

    let mut unknown: serde_json::Value =
        serde_json::from_slice(&bytes).expect("parse fixture value");
    unknown["items"][0]["judge_output"] = serde_json::json!("must stay blinded");
    assert!(serde_json::from_value::<CalibrationLabelArtifact>(unknown).is_err());
}

#[test]
fn external_memory_calibration_results_hash_pins_finite_float_canonicalization() {
    // Pins: self-hashing lexically sorts scalar keys and uses deterministic shortest finite JSON
    // numbers for nontrivial agreement, kappa, and accuracy values; NaN never hashes as null.
    let results = CalibrationResults {
        schema_version: 1,
        manifest_sha256: "a".repeat(64),
        report_sha256: "b".repeat(64),
        labeler_a_sha256: "c".repeat(64),
        labeler_b_sha256: "d".repeat(64),
        adjudication_sha256: "e".repeat(64),
        n00: 28,
        n01: 4,
        n10: 3,
        n11: 35,
        pair_denominator: 70,
        agreement: 0.9,
        kappa_status: KappaStatus::Defined,
        kappa: Some(0.798_021_434_460_016_5),
        judge_correct_count: 60,
        judge_denominator: 70,
        judge_accuracy: 0.857_142_857_142_857_1,
        agreement_pass: true,
        kappa_pass: false,
        accuracy_pass: true,
        verdict: CalibrationVerdict::Fail,
        results_sha256: "00eee88e90252d0ac2a7ff08ea2eec3bf86281a46790a6ddb6ffc433bdfcdbf1"
            .to_string(),
    };
    results
        .validate()
        .expect("known finite canonical results hash must validate");

    let mut non_finite = results;
    non_finite.agreement = f64::NAN;
    let error = non_finite
        .validate()
        .expect_err("non-finite metrics must fail before canonical hashing");
    assert!(error.to_string().contains("finite"));
}
