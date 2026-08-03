use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use moa_eval::external_memory::answer::{
    ControlKind, ExternalMemoryMode, SupportStatus, control_prerequisite,
};
use moa_eval::external_memory::cost::{
    BudgetLedger, NormalizedUsage, PricingSnapshot, StageName, UsageProvenance,
};
use moa_eval::external_memory::dataset::{
    ChronologicalTurn, DatasetFileProvenance, DatasetPackage, DatasetPackageManifest,
    DatasetPackageRegistry, DatasetPackageSource, EvidenceLabels, ExternalMemoryCase,
    ExternalMemorySession, ExternalMemoryTurn, validate_case,
};
use moa_eval::external_memory::formation::{
    ComponentConfig, ConsolidationSettings, EmbeddingConfig, EntityBlockingConfig, FormationMode,
    RecordedFormationManifest, ResolvedFormationConfig,
};
use moa_eval::external_memory::harness::{
    EvidenceExport, EvidenceOccurrenceRef, EvidenceSourceRef, ExternalMemoryBackend,
    run_retrieval_case,
};
use moa_eval::external_memory::report::{
    CaseReport, ExternalMemoryReportBuilder, FailureKind, ReaderContractV2, ReportBudgetV2,
    StageObservation,
};

fn turn(source_id: &str, hour: u32, text: &str) -> ExternalMemoryTurn {
    ExternalMemoryTurn {
        source_id: source_id.to_string(),
        occurred_at: Utc
            .with_ymd_and_hms(2026, 7, 9, hour, 0, 0)
            .single()
            .expect("fixed timestamp should parse"),
        role: "user".to_string(),
        text: text.to_string(),
    }
}

fn case() -> ExternalMemoryCase {
    ExternalMemoryCase {
        schema_version: 1,
        isolation_key: "revision-a/question-1".to_string(),
        sessions: vec![
            ExternalMemorySession {
                source_id: "session-later".to_string(),
                occurred_at: Utc
                    .with_ymd_and_hms(2026, 7, 9, 11, 0, 0)
                    .single()
                    .expect("fixed timestamp should parse"),
                turns: vec![turn("turn-later", 11, "The deployment color is blue.")],
            },
            ExternalMemorySession {
                source_id: "session-tied-a".to_string(),
                occurred_at: Utc
                    .with_ymd_and_hms(2026, 7, 9, 10, 0, 0)
                    .single()
                    .expect("fixed timestamp should parse"),
                turns: vec![turn("turn-tied-a", 10, "The owner is Ada.")],
            },
            ExternalMemorySession {
                source_id: "session-tied-b".to_string(),
                occurred_at: Utc
                    .with_ymd_and_hms(2026, 7, 9, 10, 0, 0)
                    .single()
                    .expect("fixed timestamp should parse"),
                turns: vec![turn("turn-tied-b", 10, "The region is east.")],
            },
        ],
        question: "Who owns the deployment?".to_string(),
        options: vec!["Ada".to_string(), "Lin".to_string()],
        answer: "Ada".to_string(),
        category: "single_session".to_string(),
        evidence_labels: EvidenceLabels {
            session_source_ids: Some(vec!["session-tied-a".to_string()]),
            turn_source_ids: Some(vec!["turn-tied-a".to_string()]),
        },
    }
}

fn formation() -> ResolvedFormationConfig {
    ResolvedFormationConfig {
        schema_version: 1,
        mode: FormationMode::Recorded,
        extractor: ComponentConfig {
            implementation: "recorded-v1".to_string(),
            model: Some("openai:gpt-5.4-mini".to_string()),
            prompt_version: Some("extract-v3".to_string()),
        },
        merge: ComponentConfig {
            implementation: "recorded-v1".to_string(),
            model: Some("openai:gpt-5.4-mini".to_string()),
            prompt_version: Some("merge-v2".to_string()),
        },
        embedding: EmbeddingConfig {
            provider: "gemini".to_string(),
            model: "gemini-embedding-2".to_string(),
            version: 2,
            dimensions: 3_072,
        },
        entity_blocking: EntityBlockingConfig {
            enabled: true,
            cosine_threshold: "0.91".to_string(),
        },
        pii_classifier: ComponentConfig {
            implementation: "heuristic-v1".to_string(),
            model: None,
            prompt_version: None,
        },
        contradiction_detector: ComponentConfig {
            implementation: "rrf-plus-judge-v1".to_string(),
            model: Some("openai:gpt-5.4-mini".to_string()),
            prompt_version: Some("contradiction-v1".to_string()),
        },
        consolidation: ConsolidationSettings {
            decay_idle_days: 30,
            decay_half_life_days: "180.0".to_string(),
            decay_floor: "0.1".to_string(),
            expire_idle_days: 180,
            digest_enabled: true,
            digest_max_tokens: 600,
            digest_rebuild_min_interval_hours: 1,
        },
    }
}

#[test]
fn external_memory_contract_orders_ties_and_rejects_duplicate_sources() {
    // Pins: timestamp ties retain source order, while stable source IDs are globally unique.
    let prepared = validate_case(case()).expect("valid case should prepare");
    let ordered_ids = prepared
        .chronological_turns
        .iter()
        .map(|turn| turn.turn_source_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_ids,
        vec!["turn-tied-a", "turn-tied-b", "turn-later"]
    );

    let mut duplicate = case();
    duplicate.sessions[2].turns[0].source_id = "session-tied-a".to_string();
    let error = validate_case(duplicate).expect_err("duplicate source ID must fail");
    assert!(error.to_string().contains("duplicate stable source id"));
}

#[test]
fn external_memory_labels_are_independent_and_controls_require_real_inputs() {
    // Pins: session and turn labels are independent; control support is never inferred.
    let mut session_only = case();
    session_only.evidence_labels.turn_source_ids = None;
    let prepared = validate_case(session_only).expect("session-only labels are valid");
    assert_eq!(
        control_prerequisite(&prepared, ControlKind::OracleEvidence, 8_192),
        SupportStatus::Unsupported {
            reason: "oracle evidence requires turn-level evidence labels".to_string(),
        }
    );
    assert_eq!(
        control_prerequisite(&prepared, ControlKind::NoMemory, 8_192),
        SupportStatus::Supported
    );
    assert_eq!(
        control_prerequisite(&prepared, ControlKind::FullContext, 1),
        SupportStatus::Unsupported {
            reason: "full context exceeds the reader token limit".to_string(),
        }
    );
}

#[test]
fn external_memory_formation_hash_is_schema_v1_canonical_and_complete() {
    // Pins: schema-v1 canonical hashing ignores object field order and covers all formation inputs.
    let base = formation();
    let hash = base.canonical_hash().expect("formation should hash");
    assert_eq!(
        hash,
        "c32b9926e277d139f6e60fa1af01c523f44dbc73fee412d8e4c0fb0c84bea74d"
    );
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|character| character.is_ascii_hexdigit()));

    let value = serde_json::to_value(&base).expect("serialize formation");
    let mut reversed = serde_json::Map::new();
    for (key, value) in value
        .as_object()
        .expect("formation serializes as object")
        .iter()
        .rev()
    {
        reversed.insert(key.clone(), value.clone());
    }
    assert_eq!(
        hash,
        ResolvedFormationConfig::canonical_hash_value(&serde_json::Value::Object(reversed))
            .expect("reordered formation should hash")
    );

    let pointers = [
        "/extractor/implementation",
        "/extractor/model",
        "/extractor/prompt_version",
        "/merge/implementation",
        "/merge/model",
        "/merge/prompt_version",
        "/embedding/provider",
        "/embedding/model",
        "/embedding/version",
        "/embedding/dimensions",
        "/entity_blocking/enabled",
        "/entity_blocking/cosine_threshold",
        "/pii_classifier/implementation",
        "/pii_classifier/model",
        "/pii_classifier/prompt_version",
        "/contradiction_detector/implementation",
        "/contradiction_detector/model",
        "/contradiction_detector/prompt_version",
        "/consolidation/decay_idle_days",
        "/consolidation/decay_half_life_days",
        "/consolidation/decay_floor",
        "/consolidation/expire_idle_days",
        "/consolidation/digest_enabled",
        "/consolidation/digest_max_tokens",
        "/consolidation/digest_rebuild_min_interval_hours",
    ];
    for pointer in pointers {
        let mut variant = serde_json::to_value(&base).expect("serialize formation");
        mutate_formation_leaf(&mut variant, pointer);
        let variant_hash = ResolvedFormationConfig::canonical_hash_value(&variant)
            .unwrap_or_else(|error| panic!("{pointer} mutation should remain valid: {error}"));
        assert_ne!(variant_hash, hash, "hash omitted {pointer}");
    }
    let mut mode_variant = base;
    mode_variant.mode = FormationMode::Heuristic;
    assert_ne!(
        mode_variant.canonical_hash().expect("mode should hash"),
        hash,
        "hash omitted formation mode"
    );
}

#[test]
fn external_memory_live_formation_requires_resolved_prompt_versions() {
    // Pins: a live report cannot claim a reproducible formation without both prompt identities.
    let mut live = formation();
    live.mode = FormationMode::Live;
    live.extractor.prompt_version = None;
    assert!(live.validate().is_err());

    live.extractor.prompt_version = Some("extract-v3".to_string());
    live.merge.prompt_version = Some(String::new());
    assert!(live.validate().is_err());

    live.merge.prompt_version = Some("merge-v2".to_string());
    live.validate()
        .expect("fully resolved live formation should validate");
}

fn mutate_formation_leaf(value: &mut serde_json::Value, pointer: &str) {
    let leaf = value
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("missing formation pointer {pointer}"));
    *leaf = match leaf {
        serde_json::Value::Null => serde_json::Value::String("explicit-v1".to_string()),
        serde_json::Value::Bool(value) => serde_json::Value::Bool(!*value),
        serde_json::Value::Number(value) => serde_json::json!(
            value
                .as_i64()
                .expect("formation integers fit i64")
                .saturating_add(1)
        ),
        serde_json::Value::String(value) => {
            if pointer.ends_with("cosine_threshold") {
                serde_json::Value::String("0.92".to_string())
            } else if pointer.ends_with("decay_half_life_days") {
                serde_json::Value::String("181.0".to_string())
            } else if pointer.ends_with("decay_floor") {
                serde_json::Value::String("0.2".to_string())
            } else {
                serde_json::Value::String(format!("{value}-changed"))
            }
        }
        other => panic!("unexpected formation leaf {other:?}"),
    };
}

#[test]
fn external_memory_recorded_manifest_keeps_extraction_and_merge_separate() {
    // Pins: recorded mode cannot reuse one ambiguous fixture file for both paid formation stages.
    let manifest = RecordedFormationManifest {
        schema_version: 1,
        extraction_fixture_path: "fixtures/extractions.jsonl".into(),
        extraction_fixture_sha256: "a".repeat(64),
        merge_fixture_path: "fixtures/merges.jsonl".into(),
        merge_fixture_sha256: "b".repeat(64),
    };
    manifest
        .validate()
        .expect("separate fixtures should validate");

    let mut invalid = manifest;
    invalid.merge_fixture_path = invalid.extraction_fixture_path.clone();
    let error = invalid
        .validate()
        .expect_err("ambiguous recorded fixtures must fail");
    assert!(error.to_string().contains("separate extraction and merge"));
}

#[test]
fn external_memory_package_manifest_is_versioned_and_pins_each_file() {
    // Pins: reports can identify both a dataset revision and every byte-bearing package file.
    let manifest = DatasetPackageManifest {
        schema_version: 1,
        dataset: "common-json".to_string(),
        source: DatasetPackageSource {
            repository: "fixtures/common-json".to_string(),
            revision: "fixture-v1".to_string(),
        },
        files: vec![DatasetFileProvenance {
            path: "common_cases.json".to_string(),
            size_bytes: 42,
            sha256: "c".repeat(64),
        }],
    };
    manifest.validate().expect("manifest should validate");
    assert_eq!(manifest.files.len(), 1);
    let registry = DatasetPackageRegistry::task_8();
    assert!(registry.entry("common-json").is_some());
    assert!(registry.entry("longmemeval").is_none());
}

fn pricing(model: &str, input: f64, output: f64) -> PricingSnapshot {
    PricingSnapshot {
        model: model.to_string(),
        effective_date: "2026-07-09".to_string(),
        input_per_million_usd: input,
        output_per_million_usd: output,
        cache_read_per_million_usd: input / 10.0,
        cache_write_per_million_usd: input,
    }
}

#[test]
fn external_memory_budget_is_positive_finite_and_stage_model_aware() {
    // Pins: forecasts and actual usage stay attributed to the paid stage and selected model.
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(BudgetLedger::new(invalid).is_err());
    }

    let mut ledger = BudgetLedger::new(1.0).expect("positive budget should validate");
    let forecast = NormalizedUsage {
        input_tokens_uncached: 1_000,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: 100,
        provenance: UsageProvenance::Estimated,
    };
    let reader_id = ledger
        .forecast(
            StageName::Reader,
            Some(moa_eval::external_memory::answer::ExternalMemoryMode::Primary),
            pricing("reader-model", 2.0, 8.0),
            forecast.clone(),
        )
        .expect("reader forecast should fit");
    let judge_id = ledger
        .forecast(
            StageName::Judge,
            Some(moa_eval::external_memory::answer::ExternalMemoryMode::Primary),
            pricing("judge-model", 3.0, 12.0),
            forecast,
        )
        .expect("judge forecast should fit");
    ledger
        .record_actual(
            reader_id,
            NormalizedUsage {
                input_tokens_uncached: 900,
                input_tokens_cache_write: 20,
                input_tokens_cache_read: 80,
                output_tokens: 90,
                provenance: UsageProvenance::Actual,
            },
        )
        .expect("actual reader usage should fit");
    assert_eq!(ledger.records()[reader_id].stage, StageName::Reader);
    assert_eq!(
        ledger.records()[reader_id].mode,
        Some(moa_eval::external_memory::answer::ExternalMemoryMode::Primary)
    );
    assert_eq!(ledger.records()[reader_id].pricing.model, "reader-model");
    assert_eq!(ledger.records()[judge_id].stage, StageName::Judge);
    assert!(ledger.records()[reader_id].actual_cost_usd.is_some());
    let estimated = NormalizedUsage {
        input_tokens_uncached: 1,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: 0,
        provenance: UsageProvenance::Estimated,
    };
    assert!(
        ledger
            .forecast(
                StageName::Embedding,
                Some(moa_eval::external_memory::answer::ExternalMemoryMode::Primary),
                pricing("embedding", 1.0, 0.0),
                estimated.clone(),
            )
            .is_err()
    );
    assert!(
        ledger
            .forecast(
                StageName::Reader,
                None,
                pricing("reader", 1.0, 1.0),
                estimated,
            )
            .is_err()
    );
}

#[derive(Default)]
struct RecordingBackend {
    events: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ExternalMemoryBackend for RecordingBackend {
    async fn reset(&mut self, isolation_key: &str) -> Result<(), String> {
        self.events
            .lock()
            .expect("event lock")
            .push(format!("reset:{isolation_key}"));
        Ok(())
    }

    async fn ingest(&mut self, turn: &ChronologicalTurn) -> Result<(), String> {
        self.events
            .lock()
            .expect("event lock")
            .push(format!("ingest:{}", turn.turn_source_id));
        Ok(())
    }

    async fn settle(&mut self) -> Result<(), String> {
        self.events
            .lock()
            .expect("event lock")
            .push("settle".to_string());
        Ok(())
    }

    async fn retrieve(
        &mut self,
        _query: &str,
        evidence_token_budget: usize,
        ranked_occurrence_depth: usize,
    ) -> Result<EvidenceExport, String> {
        self.events.lock().expect("event lock").push(format!(
            "retrieve:{evidence_token_budget}:{ranked_occurrence_depth}"
        ));
        Ok(EvidenceExport {
            rendered_evidence: "Ada owns the deployment.".to_string(),
            tokens_used: 6,
            ranked_source_refs: vec![EvidenceOccurrenceRef {
                session_source_id: "session-tied-a".to_string(),
                turn_source_id: "turn-tied-a".to_string(),
            }],
            rendered_source_refs: vec![EvidenceSourceRef {
                session_source_id: "session-tied-a".to_string(),
                turn_source_id: "turn-tied-a".to_string(),
                evidence: "Ada owns the deployment.".to_string(),
            }],
        })
    }
}

#[tokio::test]
async fn external_memory_harness_resets_orders_ingest_and_preserves_source_ids() {
    // Pins: each case resets first, ingests chronologically, settles, then retrieves with the exact budget.
    let prepared = validate_case(case()).expect("case should validate");
    let mut backend = RecordingBackend::default();
    let events = backend.events.clone();
    let evidence = run_retrieval_case(&mut backend, &prepared, 32, 4)
        .await
        .expect("fake backend should run");
    assert_eq!(evidence.tokens_used, 6);
    assert_eq!(
        evidence.rendered_source_refs[0].turn_source_id,
        "turn-tied-a"
    );
    assert_eq!(
        *events.lock().expect("event lock"),
        vec![
            "reset:revision-a/question-1",
            "ingest:turn-tied-a",
            "ingest:turn-tied-b",
            "ingest:turn-later",
            "settle",
            "retrieve:32:4",
        ]
    );
}

#[tokio::test]
async fn external_memory_harness_rejects_backend_evidence_over_budget() {
    // Pins: a backend cannot claim success after exporting evidence over the requested budget.
    struct OverBudgetBackend;

    #[async_trait]
    impl ExternalMemoryBackend for OverBudgetBackend {
        async fn reset(&mut self, _isolation_key: &str) -> Result<(), String> {
            Ok(())
        }

        async fn ingest(&mut self, _turn: &ChronologicalTurn) -> Result<(), String> {
            Ok(())
        }

        async fn settle(&mut self) -> Result<(), String> {
            Ok(())
        }

        async fn retrieve(
            &mut self,
            _query: &str,
            evidence_token_budget: usize,
            _ranked_occurrence_depth: usize,
        ) -> Result<EvidenceExport, String> {
            Ok(EvidenceExport {
                rendered_evidence: "over budget".to_string(),
                tokens_used: evidence_token_budget + 1,
                ranked_source_refs: Vec::new(),
                rendered_source_refs: Vec::new(),
            })
        }
    }

    let prepared = validate_case(case()).expect("case should validate");
    let error = run_retrieval_case(&mut OverBudgetBackend, &prepared, 8, 4)
        .await
        .expect_err("over-budget evidence must fail");
    assert!(
        error
            .to_string()
            .contains("used 9 evidence tokens with budget 8")
    );
}

#[test]
fn external_memory_report_serialization_is_clock_normalized_and_keeps_failures() {
    // Pins: nearest-rank percentiles, explicit denominators, partial failures, and map ordering are deterministic.
    let generated_at = Utc
        .with_ymd_and_hms(2026, 7, 9, 12, 0, 0)
        .single()
        .expect("fixed timestamp should parse");
    let package = DatasetPackage::new(DatasetPackageManifest {
        schema_version: 1,
        dataset: "common-json".to_string(),
        source: DatasetPackageSource {
            repository: "fixtures/common-json".to_string(),
            revision: "fixture-v1".to_string(),
        },
        files: vec![DatasetFileProvenance {
            path: "common_cases.json".to_string(),
            size_bytes: 42,
            sha256: "c".repeat(64),
        }],
    })
    .expect("package should hash");
    let formation = formation();
    let formation_hash = formation.canonical_hash().expect("formation should hash");
    let mut builder = ExternalMemoryReportBuilder::new(
        generated_at,
        package,
        formation,
        formation_hash,
        ReaderContractV2::new("fixture:reader", "reader-v1", 4096, 64),
        ReportBudgetV2 {
            ceiling_usd: 1.0,
            estimated_committed_usd: 0.0,
            actual_or_estimated_committed_usd: 0.0,
        },
    );
    for latency_ms in [10, 20, 30, 40, 50] {
        builder.record_stage(StageObservation {
            stage: StageName::Retrieval,
            mode: Some(ExternalMemoryMode::Primary),
            latency_ms,
            accounting: None,
        });
    }
    builder.record_case(CaseReport::failed(
        "revision-a/question-1",
        "single_session",
        FailureKind::Timeout,
        "reader timed out",
    ));
    builder.record_case(CaseReport::completed(
        "revision-a/question-2",
        "temporal",
        "evidence",
        SupportStatus::Supported,
    ));
    for mode in [
        ExternalMemoryMode::NoMemory,
        ExternalMemoryMode::FullContext,
        ExternalMemoryMode::OracleEvidence,
    ] {
        builder.record_case(CaseReport::unsupported(
            "revision-a/question-1",
            "single_session",
            mode,
            "fixture-control",
        ));
        builder.record_case(CaseReport::unsupported(
            "revision-a/question-2",
            "temporal",
            mode,
            "fixture-control",
        ));
    }
    let report = builder.finish();
    let primary = &report.modes[0];
    assert_eq!(primary.denominators.total_cases, 2);
    assert_eq!(primary.denominators.completed_cases, 1);
    assert_eq!(primary.denominators.failed_cases, 1);
    let retrieval = report
        .stage_metrics
        .iter()
        .find(|metrics| {
            metrics.stage == StageName::Retrieval
                && metrics.mode == Some(ExternalMemoryMode::Primary)
        })
        .expect("primary retrieval metrics");
    assert_eq!(retrieval.p50_latency_ms, 30);
    assert_eq!(retrieval.p95_latency_ms, 50);
    assert_eq!(
        primary
            .cases
            .iter()
            .filter(|case| case.failure.is_some())
            .count(),
        1
    );

    let first = report.canonical_json().expect("report should serialize");
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("report JSON should parse");
    let second = report
        .canonical_json()
        .expect("report should serialize twice");
    assert_eq!(first, second);
    assert_eq!(parsed["generated_at"], "2026-07-09T12:00:00Z");
    assert_eq!(
        primary.category_slices,
        BTreeMap::from([
            ("single_session".to_string(), 1),
            ("temporal".to_string(), 1),
        ])
    );
}

#[test]
fn external_memory_neutral_modules_only_use_the_shared_canonical_primitive() {
    // Pins: benchmark contracts stay independent of MOA backends while sharing the workspace's
    // one canonical-byte contract instead of maintaining another serializer.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/external_memory");
    for name in [
        "answer.rs",
        "cost.rs",
        "dataset.rs",
        "formation.rs",
        "harness.rs",
        "report.rs",
    ] {
        let source = std::fs::read_to_string(root.join(name)).expect("read neutral module");
        let moa_imports = source
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with("use moa_"))
            .collect::<Vec<_>>();
        assert!(
            moa_imports
                .iter()
                .all(|line| *line == "use moa_core::canonical_json::canonical_json_bytes;"),
            "{name} must not import MOA backend/runtime crates: {moa_imports:?}"
        );
    }
}
