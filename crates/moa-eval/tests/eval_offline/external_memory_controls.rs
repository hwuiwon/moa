//! Hermetic Task 11 control rendering, fitting, and V2 wire tests.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use moa_eval::external_memory::answer::{
    ExternalMemoryMode, FULL_CONTEXT_V1_PREFIX, PERSONAMEM_ORACLE_UNSUPPORTED_REASON,
    READER_CONTEXT_LIMIT_REASON, SupportStatus, reader_fit_support, render_control_evidence,
    render_reader_prompt,
};
use moa_eval::external_memory::dataset::{
    DatasetFileProvenance, DatasetPackageManifestV1, DatasetPackageSourceV1, DatasetPackageV1,
    EvidenceLabels, ExternalMemoryCaseV1, ExternalMemorySession, ExternalMemoryTurn, validate_case,
};
use moa_eval::external_memory::formation::{
    ComponentConfig, ConsolidationSettings, EmbeddingConfig, EntityBlockingConfig, FormationMode,
    ResolvedFormationConfig,
};
use moa_eval::external_memory::longmemeval::LONGMEMEVAL_DATASET;
use moa_eval::external_memory::personamem::{PERSONAMEM_DATASET, PersonaMemAccuracyReportV1};
use moa_eval::external_memory::report::{
    CaseReportV2, ExternalMemoryDatasetMetricsV2, ExternalMemoryReportBuilder,
    PersonaMemModeMetricsV2, ReaderContractV2, ReportBudgetV2,
};
use moa_eval::kernel::stats::ClusterBootstrapReport;

fn prepared(
    labels: Option<Vec<String>>,
) -> moa_eval::external_memory::dataset::PreparedExternalMemoryCase {
    validate_case(ExternalMemoryCaseV1 {
        schema_version: 1,
        isolation_key: "case-1".to_string(),
        sessions: vec![ExternalMemorySession {
            source_id: "session-1".to_string(),
            occurred_at: Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
            turns: vec![
                ExternalMemoryTurn {
                    source_id: "turn-1".to_string(),
                    occurred_at: Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 1).unwrap(),
                    role: "user".to_string(),
                    text: "Remember café blue.".to_string(),
                },
                ExternalMemoryTurn {
                    source_id: "turn-2".to_string(),
                    occurred_at: Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 2).unwrap(),
                    role: "assistant".to_string(),
                    text: "Acknowledged.".to_string(),
                },
            ],
        }],
        question: "What color?".to_string(),
        options: vec!["red".to_string(), "blue".to_string()],
        answer: "blue".to_string(),
        category: "single-session-user".to_string(),
        evidence_labels: EvidenceLabels {
            session_source_ids: Some(vec!["session-1".to_string()]),
            turn_source_ids: labels,
        },
    })
    .expect("fixture is valid")
}

#[test]
fn external_memory_controls_render_exact_envelopes_without_truncation() {
    // Pins: no-memory is empty and full/oracle envelopes retain exact source-order DTOs.
    let case = prepared(Some(vec!["turn-1".to_string()]));
    let no_memory =
        render_control_evidence(&case, ExternalMemoryMode::NoMemory, LONGMEMEVAL_DATASET)
            .expect("no-memory renders");
    assert_eq!(no_memory.rendered_evidence, "");
    assert_eq!(no_memory.rendered_evidence_tokens, 0);

    let full = render_control_evidence(&case, ExternalMemoryMode::FullContext, LONGMEMEVAL_DATASET)
        .expect("full context renders");
    assert!(full.rendered_evidence.starts_with(FULL_CONTEXT_V1_PREFIX));
    let full_json: serde_json::Value = serde_json::from_str(
        full.rendered_evidence
            .strip_prefix(FULL_CONTEXT_V1_PREFIX)
            .expect("prefix"),
    )
    .expect("compact envelope JSON");
    assert_eq!(full_json["mode"], "full_context");
    assert_eq!(
        full_json["sessions"][0]["turns"].as_array().unwrap().len(),
        2
    );
    assert!(
        !full
            .rendered_evidence
            .strip_prefix(FULL_CONTEXT_V1_PREFIX)
            .expect("prefix")
            .contains('\n')
    );

    let oracle = render_control_evidence(
        &case,
        ExternalMemoryMode::OracleEvidence,
        LONGMEMEVAL_DATASET,
    )
    .expect("oracle renders");
    let oracle_json: serde_json::Value = serde_json::from_str(
        oracle
            .rendered_evidence
            .strip_prefix(FULL_CONTEXT_V1_PREFIX)
            .expect("prefix"),
    )
    .expect("compact envelope JSON");
    assert_eq!(oracle_json["mode"], "oracle_evidence");
    assert_eq!(
        oracle_json["sessions"][0]["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        oracle_json["sessions"][0]["turns"][0]["source_id"],
        "turn-1"
    );
}

#[test]
fn external_memory_controls_reject_or_mark_missing_oracle_prerequisites() {
    // Pins: the control never guesses labels and PersonaMem always uses the exact exclusion.
    let case = prepared(None);
    assert!(
        render_control_evidence(
            &case,
            ExternalMemoryMode::OracleEvidence,
            LONGMEMEVAL_DATASET,
        )
        .is_err()
    );
    let persona = render_control_evidence(
        &case,
        ExternalMemoryMode::OracleEvidence,
        PERSONAMEM_DATASET,
    )
    .expect("PersonaMem is a precomputed unsupported case");
    assert_eq!(
        persona.support,
        SupportStatus::Unsupported {
            reason: PERSONAMEM_ORACLE_UNSUPPORTED_REASON.to_string(),
        }
    );
}

#[test]
fn external_memory_controls_fit_the_exact_shared_reader_request() {
    // Pins: Unicode scalar count, prompt version/options/evidence, output reserve, and no truncation.
    let case = prepared(Some(vec!["turn-1".to_string()]));
    let evidence =
        render_control_evidence(&case, ExternalMemoryMode::FullContext, LONGMEMEVAL_DATASET)
            .expect("full context renders");
    let prompt = render_reader_prompt(
        &case,
        &evidence.rendered_evidence,
        "reader-v7",
        LONGMEMEVAL_DATASET,
    );
    assert!(prompt.system.contains("reader-v7"));
    assert!(prompt.user.contains("1. red\n2. blue"));
    assert!(prompt.user.ends_with(&evidence.rendered_evidence));
    let manual = (prompt.system.chars().count() + prompt.user.chars().count()).div_ceil(4) as u64;
    assert_eq!(prompt.estimated_input_tokens(), manual);
    assert_eq!(
        reader_fit_support(&prompt, manual + 31, 32),
        SupportStatus::Unsupported {
            reason: READER_CONTEXT_LIMIT_REASON.to_string(),
        }
    );
    assert_eq!(
        reader_fit_support(&prompt, manual + 32, 32),
        SupportStatus::Supported
    );
}

#[test]
fn external_memory_controls_v2_report_is_strict_and_mode_ordered() {
    // Pins: the hard-break schema retains four ordered full denominators and rejects unknown fields.
    let formation = ResolvedFormationConfig {
        schema_version: 1,
        mode: FormationMode::Heuristic,
        extractor: component("heuristic"),
        merge: component("deterministic"),
        embedding: EmbeddingConfig {
            provider: "fixture".to_string(),
            model: "fixture-embed".to_string(),
            version: 1,
            dimensions: 3,
        },
        entity_blocking: EntityBlockingConfig {
            enabled: false,
            cosine_threshold: "0.8".to_string(),
        },
        pii_classifier: component("heuristic"),
        contradiction_detector: component("heuristic"),
        consolidation: ConsolidationSettings {
            decay_idle_days: 30,
            decay_half_life_days: "30".to_string(),
            decay_floor: "0.1".to_string(),
            expire_idle_days: 365,
            digest_enabled: false,
            digest_max_tokens: 128,
            digest_rebuild_min_interval_hours: 24,
        },
    };
    let formation_hash = formation.canonical_hash().expect("formation hashes");
    let package = DatasetPackageV1::new(DatasetPackageManifestV1 {
        schema_version: 1,
        dataset: "common-json".to_string(),
        source: DatasetPackageSourceV1 {
            repository: "fixture".to_string(),
            revision: "v1".to_string(),
        },
        files: vec![DatasetFileProvenance {
            path: "cases.json".to_string(),
            size_bytes: 1,
            sha256: "a".repeat(64),
        }],
    })
    .expect("package hashes");
    let mut builder = ExternalMemoryReportBuilder::new(
        Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
        package,
        formation,
        formation_hash,
        ReaderContractV2::new("openai:reader", "reader-v1", 4096, 64),
        ReportBudgetV2 {
            ceiling_usd: 1.0,
            estimated_committed_usd: 0.0,
            actual_or_estimated_committed_usd: 0.0,
        },
    );
    for mode in ExternalMemoryMode::ordered() {
        builder.record_case(CaseReportV2::completed_for_mode(
            "case-1",
            "single-session-user",
            mode,
            "",
            0,
            SupportStatus::Supported,
        ));
    }
    builder.set_dataset_metrics(
        ExternalMemoryMode::Primary,
        ExternalMemoryDatasetMetricsV2::PersonaMem32k(Box::new(PersonaMemModeMetricsV2 {
            answer: PersonaMemAccuracyReportV1 {
                schema_version: 1,
                metric: "personamem_label_accuracy_v1".to_string(),
                numerator: 1,
                denominator: 1,
                cluster_count: 1,
                bootstrap: ClusterBootstrapReport {
                    metric_name: "personamem_label_accuracy_v1".to_string(),
                    resamples: 10,
                    seed: 7,
                    cluster_count: 1,
                    observation_count: 1,
                    mean: 1.0,
                    lower_percentile: 2.5,
                    lower: 1.0,
                    upper_percentile: 97.5,
                    upper: 1.0,
                },
                question_type_slices: BTreeMap::new(),
                distance_slices: BTreeMap::new(),
                retrieval_recall: SupportStatus::Unsupported {
                    reason: "not-labeled".to_string(),
                },
            },
            retrieval: SupportStatus::Unsupported {
                reason: "not-labeled".to_string(),
            },
        })),
    );
    let report = builder.finish();
    assert_eq!(report.schema_version, 2);
    assert_eq!(
        report
            .modes
            .iter()
            .map(|mode| mode.mode)
            .collect::<Vec<_>>(),
        ExternalMemoryMode::ordered()
    );
    assert!(
        report
            .modes
            .iter()
            .all(|mode| mode.denominators.total_cases == 1)
    );
    let mut nested_unknown = serde_json::to_value(&report).expect("serialize report");
    nested_unknown["modes"][0]["dataset_metrics"]
        .as_object_mut()
        .expect("adjacently tagged metrics object")
        .insert("unknown".to_string(), true.into());
    assert!(
        serde_json::from_value::<moa_eval::external_memory::report::ExternalMemoryReportV2>(
            nested_unknown,
        )
        .is_err()
    );

    let mut value = serde_json::to_value(&report).expect("serialize report");
    value
        .as_object_mut()
        .unwrap()
        .insert("unknown".to_string(), true.into());
    assert!(
        serde_json::from_value::<moa_eval::external_memory::report::ExternalMemoryReportV2>(value)
            .is_err()
    );
    assert!(report.canonical_json().is_ok());
}

fn component(implementation: &str) -> ComponentConfig {
    ComponentConfig {
        implementation: implementation.to_string(),
        model: None,
        prompt_version: None,
    }
}
