use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::planning::parse_temporal;
use moa_brain::retrieval::{LegSources, RetrievalHit};
use moa_core::{MoaError, SessionId, StoragePartitionId, UserId, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_eval::kernel::{CostLedger, ProviderProvenance};
use moa_eval::memory_eval::runner::QueryRewriteClassMetrics;
use moa_eval::memory_eval::{
    BinaryProbeOutcome, BootstrapConfig, CORPUS_SCHEMA_VERSION, CachedEmbeddingProvider,
    CandidateLegs, CorpusManifest, CorpusProfile, EmbeddingInputKind, EntityFragmentationCounts,
    ExtractionPrecisionCounts, GeneratedMemoryEvalCorpus, GoldNodeRecord, GoldPiiStatus,
    GoldResolutionReport, GoldResolutionStatus, LedgerFact, MemoryRetrievalEvalOptions,
    MemoryRetrievalEvalReport, MetricSummary, Probe, ProbeResult, ProbeType, QueryRewritePolicy,
    RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K, RetrievedCandidate, SyntheticSession,
    SyntheticTurn, TranscriptStyle, aggregate_retrieval_eval_from_counts,
    aggregate_retrieval_eval_from_diagnostic_counts, aggregate_retrieval_eval_with_diagnostics,
    aggregate_retrieval_eval_with_extraction_precision, benjamini_hochberg,
    build_cached_embedding_fixtures, candidates_from_retrieval_hits, embedding_text_hash,
    generate_memory_eval_corpus, generate_memory_eval_corpus_with_style, mcnemar_paired_test,
    read_embedding_inputs_jsonl, read_embeddings_jsonl, read_gold_nodes_jsonl, read_ledger_jsonl,
    read_manifest_json, read_probes_jsonl, read_sessions_jsonl, resolve_gold_nodes,
    run_memory_retrieval_eval, stable_uuid_from_label, tenant_id_from_storage_partition_id,
    validate_corpus, write_embeddings_jsonl, write_gold_nodes_jsonl, write_ledger_jsonl,
    write_manifest_json, write_memory_eval_corpus, write_probes_jsonl, write_sessions_jsonl,
};
use moa_eval_core::EvalError;
use moa_memory_graph::{AgeGraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, IngestCtx, IngestError,
};
use moa_memory_pii::{PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_types::{ScopeContext, ScopeTier};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_session::testing;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

static GOLD_RESOLUTION_TEST_LOCK: Mutex<()> = Mutex::const_new(());
const GOLD_RESOLUTION_EMBEDDER_MODEL: &str = "gold-resolution-mock-embedder";
const GOLD_RESOLUTION_EMBEDDER_VERSION: i32 = 31;

#[tokio::test]
async fn memory_eval_corpus_round_trips_versioned_jsonl() {
    // Pins: memory eval corpus files preserve scoped, temporal, PII, and probe metadata.
    let (manifest, facts, sessions, probes) = realistic_corpus();
    let temp = tempfile::tempdir().expect("create temp corpus directory");
    let manifest_path = temp.path().join("manifest.json");
    let ledger_path = temp.path().join("ledger.jsonl");
    let sessions_path = temp.path().join("sessions.jsonl");
    let probes_path = temp.path().join("probes.jsonl");

    write_manifest_json(&manifest_path, &manifest)
        .await
        .expect("write manifest");
    write_ledger_jsonl(&ledger_path, &facts)
        .await
        .expect("write ledger jsonl");
    write_sessions_jsonl(&sessions_path, &sessions)
        .await
        .expect("write sessions jsonl");
    write_probes_jsonl(&probes_path, &probes, &facts)
        .await
        .expect("write probes jsonl");

    let round_tripped_manifest = read_manifest_json(&manifest_path)
        .await
        .expect("read manifest");
    let round_tripped_facts = read_ledger_jsonl(&ledger_path)
        .await
        .expect("read ledger jsonl");
    let round_tripped_sessions = read_sessions_jsonl(&sessions_path)
        .await
        .expect("read sessions jsonl");
    let round_tripped_probes = read_probes_jsonl(&probes_path, &round_tripped_facts)
        .await
        .expect("read probes jsonl");

    validate_corpus(
        &round_tripped_manifest,
        &round_tripped_facts,
        &round_tripped_sessions,
        &round_tripped_probes,
    )
    .expect("round-tripped corpus validates");

    assert_eq!(round_tripped_manifest, manifest);
    assert_eq!(round_tripped_facts, facts);
    assert_eq!(round_tripped_sessions, sessions);
    assert_eq!(round_tripped_probes, probes);

    let probes_jsonl = tokio::fs::read_to_string(&probes_path)
        .await
        .expect("read probes jsonl text");
    for probe_type in [
        "point_recall",
        "latest_value_after_update",
        "abstention",
        "cross_user_isolation",
        "tenant_shared_fact",
        "multi_hop",
        "temporal_as_of",
        "preference_application",
        "pii_redaction",
    ] {
        assert!(
            probes_jsonl.contains(probe_type),
            "serialized probes should include {probe_type}"
        );
    }
}

#[tokio::test]
async fn memory_eval_corpus_rejects_cross_user_probe_owned_by_asking_user() {
    // Pins: cross-user isolation probes must reference another user's private fact.
    let (_, facts, _, _) = realistic_corpus();
    let temp = tempfile::tempdir().expect("create temp corpus directory");
    let probes_path = temp.path().join("probes.jsonl");
    let bad_probe = Probe {
        probe_id: "probe-cross-user-bad-owner".to_string(),
        probe_type: ProbeType::CrossUserIsolation,
        storage_partition_id: storage_partition("tenant-payments"),
        user_id: user("user-bob"),
        query: "What editor does Bob prefer?".to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: "The assistant should abstain instead of exposing Bob's private preference."
            .to_string(),
        expected_fact_ids: Vec::new(),
        blocked_fact_ids: vec!["fact-bob-editor".to_string()],
        as_of: None,
        expected_redacted: false,
    };

    let error = write_probes_jsonl(&probes_path, &[bad_probe], &facts)
        .await
        .expect_err("cross-user probe owned by asking user should fail validation");

    match error {
        EvalError::InvalidConfig(message) => {
            assert!(
                message.contains("cross-user isolation")
                    && message.contains("probe-cross-user-bad-owner")
                    && message.contains("fact-bob-editor"),
                "error should identify the invalid cross-user probe: {message}"
            );
        }
        other => panic!("expected EvalError::InvalidConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn memory_eval_pr_generator_writes_byte_stable_ledger_first_corpus() {
    // Pins: PR corpus generation is deterministic and includes every ledger-first fact class.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    assert_profile_shape(&corpus, 5, 2, 60..=usize::MAX);
    assert_ledger_first_fact_classes(&corpus.ledger);

    let temp = tempfile::tempdir().expect("create temp corpus root");
    let first_dir = temp.path().join("pr-a");
    let second_dir = temp.path().join("pr-b");
    write_memory_eval_corpus(&first_dir, &corpus)
        .await
        .expect("write first generated corpus");
    write_memory_eval_corpus(
        &second_dir,
        &generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
            .expect("regenerate PR memory eval corpus"),
    )
    .await
    .expect("write second generated corpus");

    for file_name in [
        "manifest.json",
        "ledger.jsonl",
        "sessions.jsonl",
        "probes.jsonl",
        "embedding_inputs.jsonl",
    ] {
        let first = tokio::fs::read(first_dir.join(file_name))
            .await
            .expect("read first generated file");
        let second = tokio::fs::read(second_dir.join(file_name))
            .await
            .expect("read second generated file");
        assert_eq!(first, second, "{file_name} should be byte-stable");
    }

    let manifest = read_manifest_json(&first_dir.join("manifest.json"))
        .await
        .expect("read generated manifest");
    let ledger = read_ledger_jsonl(&first_dir.join("ledger.jsonl"))
        .await
        .expect("read generated ledger");
    let sessions = read_sessions_jsonl(&first_dir.join("sessions.jsonl"))
        .await
        .expect("read generated sessions");
    let probes = read_probes_jsonl(&first_dir.join("probes.jsonl"), &ledger)
        .await
        .expect("read generated probes");
    let embedding_inputs =
        read_embedding_inputs_jsonl(&first_dir.join("embedding_inputs.jsonl"), &ledger, &probes)
            .await
            .expect("read generated embedding inputs");

    validate_corpus(&manifest, &ledger, &sessions, &probes).expect("generated corpus validates");
    assert!(
        probes.iter().all(|probe| probe.query_class.is_some()
            && probe.expected_rewrite.is_some()
            && probe.rewrite_query.is_some()),
        "each generated probe should carry deterministic query-rewrite fixtures"
    );
    assert!(
        probes.iter().any(|probe| probe
            .rewrite_query
            .as_ref()
            .is_some_and(|rewrite| rewrite != &probe.query)),
        "at least one generated rewrite fixture should differ from the original query"
    );
    assert!(
        embedding_inputs.len() > ledger.len() + probes.len(),
        "embedding inputs should include original probes plus rewrite fixtures"
    );
    assert_eq!(
        embedding_inputs
            .iter()
            .filter(|input| input.kind == EmbeddingInputKind::Fact)
            .count(),
        ledger.len()
    );
    assert!(
        embedding_inputs
            .iter()
            .filter(|input| input.kind == EmbeddingInputKind::Probe)
            .count()
            > probes.len(),
        "probe embedding inputs should include rewrite fixture variants"
    );
}

#[test]
fn generator_emits_four_temporal_variants_per_supersession_chain() {
    // Pins: each PR tenant supersession chain emits four absolute-date temporal probes.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    let temporal = corpus
        .probes
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::TemporalAsOf)
        .collect::<Vec<_>>();

    assert_eq!(temporal.len(), 24);
    for suffix in ["month", "date", "current", "back-in"] {
        assert_eq!(
            temporal
                .iter()
                .filter(|probe| probe.probe_id.ends_with(suffix))
                .count(),
            6,
            "expected one `{suffix}` temporal probe per seed/tenant chain"
        );
    }
    assert!(
        temporal.iter().all(|probe| probe.as_of.is_some()),
        "each temporal probe should carry the instant encoded in query text"
    );
    for probe in temporal {
        assert_eq!(
            parse_temporal(&probe.query),
            probe.as_of,
            "temporal parser should recover generator query date for {}",
            probe.probe_id
        );
    }
}

#[test]
fn manifest_round_trips_transcript_style_and_defaults_to_marked() -> TestResult {
    // Pins: prompt-02-era manifests remain readable and new manifests preserve transcript style.
    let old_manifest = serde_json::json!({
        "version": CORPUS_SCHEMA_VERSION,
        "corpus_id": "memory-eval-pr-minimal",
        "profile": "pr",
        "description": "manifest without transcript style",
        "seeds": [1, 2, 3]
    });
    let parsed_old: CorpusManifest = serde_json::from_value(old_manifest)?;
    assert_eq!(parsed_old.transcript_style, TranscriptStyle::Marked);

    let natural_manifest = serde_json::json!({
        "version": CORPUS_SCHEMA_VERSION,
        "corpus_id": "memory-eval-pr-natural-1-2-3",
        "profile": "pr",
        "description": "natural manifest",
        "seeds": [1, 2, 3],
        "transcript_style": "natural"
    });
    let parsed_natural: CorpusManifest = serde_json::from_value(natural_manifest)?;
    assert_eq!(parsed_natural.transcript_style, TranscriptStyle::Natural);
    Ok(())
}

#[test]
fn natural_transcripts_contain_no_fact_markers() {
    // Pins: natural transcripts do not use marker tokens the heuristic extractor was tuned for.
    let corpus = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");

    for turn in corpus.sessions.iter().flat_map(|session| &session.turns) {
        for forbidden in ["Fact:", "tenant shared", "contact private"] {
            assert!(
                !turn.transcript.contains(forbidden),
                "natural transcript should not contain marker `{forbidden}`: {}",
                turn.transcript
            );
        }
    }
    assert!(
        corpus
            .sessions
            .iter()
            .all(|session| session.turns.iter().any(|turn| turn.fact_ids.is_empty())),
        "each natural session should include at least one distractor turn"
    );
}

#[test]
fn natural_generation_is_deterministic_for_same_seed() {
    // Pins: natural corpus generation is byte-stable for the same profile and seeds.
    let first = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate first natural corpus");
    let second = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate second natural corpus");

    assert_eq!(first, second);
}

#[test]
fn corpus_id_encodes_transcript_style() {
    // Pins: marked and natural corpora have distinct identities for paired comparison.
    let marked = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate marked PR corpus");
    let natural = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");

    assert_eq!(marked.manifest.corpus_id, "memory-eval-pr-marked-1-2-3");
    assert_eq!(natural.manifest.corpus_id, "memory-eval-pr-natural-1-2-3");
    assert_ne!(marked.manifest.corpus_id, natural.manifest.corpus_id);
}

#[test]
fn natural_frames_cover_every_generated_predicate() {
    // Pins: generated predicates stay inside the deterministic natural phrase table contract.
    let corpus = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");
    let predicates = corpus
        .ledger
        .iter()
        .map(|fact| fact.predicate.as_str())
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        "cache_backend_conflict",
        "contact_email",
        "depends_on",
        "deploy_target",
        "on_call_primary",
        "owned_by",
        "private_repository",
        "require_runbook",
        "response_style",
    ]);

    assert_eq!(predicates, expected);
}

#[test]
fn multi_hop_templates_emit_two_expected_fact_ids_sharing_entity() {
    // Pins: multi-hop probes require a dependency fact and an owner fact linked by library.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate marked PR corpus");
    let facts = corpus
        .ledger
        .iter()
        .map(|fact| (fact.fact_id.as_str(), fact))
        .collect::<HashMap<_, _>>();

    for probe in corpus
        .probes
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::MultiHop)
    {
        assert_eq!(probe.expected_fact_ids.len(), 2);
        let dependency = facts
            .get(probe.expected_fact_ids[0].as_str())
            .expect("dependency fact exists");
        let owner = facts
            .get(probe.expected_fact_ids[1].as_str())
            .expect("owner fact exists");
        assert_eq!(dependency.predicate, "depends_on");
        assert_eq!(owner.predicate, "owned_by");
        assert_eq!(dependency.object, owner.subject);
        assert_ne!(dependency.source_session_id, owner.source_session_id);
    }
}

#[test]
fn pr_profile_emits_at_least_thirty_multi_hop_probes() {
    // Pins: prompt 04 has enough multi-hop probes for a statistical graph-leg gate.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate marked PR corpus");
    let multi_hop_count = corpus
        .probes
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::MultiHop)
        .count();

    assert!(
        multi_hop_count >= 30,
        "PR profile should emit at least 30 multi-hop probes, got {multi_hop_count}"
    );
}

#[tokio::test]
async fn cached_embedding_provider_returns_fixture_vectors_and_missing_hash_errors() {
    // Pins: cached embeddings are hermetic, dimension-checked, order-preserving, and fail closed.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    let fixtures = build_cached_embedding_fixtures(&corpus.embedding_inputs)
        .expect("build deterministic cached embedding fixtures");
    assert!(
        fixtures
            .iter()
            .all(|fixture| fixture.dimension == VECTOR_DIMENSION
                && fixture.vector.len() == VECTOR_DIMENSION),
        "every cached fixture should match moa_memory_vector::VECTOR_DIMENSION"
    );

    let temp = tempfile::tempdir().expect("create temp embedding fixture directory");
    let embeddings_path = temp.path().join("embeddings.jsonl");
    write_embeddings_jsonl(&embeddings_path, &fixtures)
        .await
        .expect("write cached embeddings jsonl");

    let serialized = tokio::fs::read_to_string(&embeddings_path)
        .await
        .expect("read embeddings jsonl text");
    assert!(
        serialized.contains("\"text_hash\"")
            && serialized.contains("\"model\"")
            && serialized.contains("\"dimension\"")
            && serialized.contains("\"vector\""),
        "embeddings.jsonl should preserve the frozen fixture fields"
    );

    let loaded_fixtures = read_embeddings_jsonl(&embeddings_path)
        .await
        .expect("read cached embeddings jsonl");
    let provider = CachedEmbeddingProvider::from_jsonl(&embeddings_path)
        .await
        .expect("load cached embedding provider");
    assert_eq!(provider.dimensions(), VECTOR_DIMENSION);

    let first_input = corpus
        .embedding_inputs
        .first()
        .expect("generated corpus has embedding inputs");
    let last_input = corpus
        .embedding_inputs
        .last()
        .expect("generated corpus has embedding inputs");
    let request = vec![last_input.text.clone(), first_input.text.clone()];
    let embeddings = provider
        .embed(&request)
        .await
        .expect("embed from cached fixtures");
    assert_eq!(embeddings.len(), 2);
    assert_eq!(
        embeddings[0],
        fixture_vector(&loaded_fixtures, &last_input.text)
    );
    assert_eq!(
        embeddings[1],
        fixture_vector(&loaded_fixtures, &first_input.text)
    );

    let missing_text = "this text intentionally has no cached fixture".to_string();
    let missing_hash = embedding_text_hash(&missing_text);
    let error = provider
        .embed(&[missing_text])
        .await
        .expect_err("missing cached fixture should fail closed");
    match error {
        MoaError::ProviderError(message) => assert!(
            message.contains(&missing_hash),
            "missing fixture error should name text_hash {missing_hash}: {message}"
        ),
        other => panic!("expected MoaError::ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn gold_resolution_reports_partial_and_full_ingestion_coverage() -> TestResult {
    // Pins: gold resolution ingests real turns and distinguishes explicit facts from unextractable ledger facts.
    let _guard = GOLD_RESOLUTION_TEST_LOCK.lock().await;

    let explicit_stack = GoldResolutionStack::up().await?;
    let explicit_result = run_explicit_gold_resolution_case(&explicit_stack).await;
    let explicit_cleanup = explicit_stack.cleanup().await;
    explicit_result?;
    explicit_cleanup?;

    let partial_stack = GoldResolutionStack::up().await?;
    let partial_result = run_partial_gold_resolution_case(&partial_stack).await;
    let partial_cleanup = partial_stack.cleanup().await;
    partial_result?;
    partial_cleanup
}

#[tokio::test]
async fn memory_retrieval_eval_runner_writes_report_from_cached_embeddings() -> TestResult {
    // Pins: retrieval eval uses cached embeddings, resolves gold nodes, collects top-25 candidates, and writes the report sections.
    if std::env::var_os("MOA_DATABASE_URL").is_none() {
        return Ok(());
    }

    let _guard = GOLD_RESOLUTION_TEST_LOCK.lock().await;
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    let temp = tempfile::tempdir()?;
    let corpus_dir = temp.path().join("pr-corpus");
    write_memory_eval_corpus(&corpus_dir, &corpus).await?;
    let embeddings = build_cached_embedding_fixtures(&corpus.embedding_inputs)
        .expect("build cached embedding fixtures");
    write_embeddings_jsonl(&corpus_dir.join("embeddings.jsonl"), &embeddings).await?;

    let report_path = temp.path().join("report.json");
    let report = run_memory_retrieval_eval(
        MemoryRetrievalEvalOptions::new(&corpus_dir, &report_path).with_bootstrap_config(
            BootstrapConfig {
                resamples: 200,
                seed: 29,
            },
        ),
    )
    .await?;

    assert_eq!(report.candidate_k, RETRIEVAL_EVAL_CANDIDATE_K);
    assert_eq!(report.final_k, RETRIEVAL_EVAL_FINAL_K);
    assert!(!report.reranker_enabled);
    assert_eq!(report.probe_results.len(), corpus.probes.len());
    assert!(!report.gold_resolution.records.is_empty());
    assert!(
        report.metrics.recall_at_25.denominator > 0,
        "report should include non-empty retrieval metrics"
    );
    assert!(
        report
            .probe_results
            .iter()
            .flat_map(|probe| probe.candidates.iter())
            .all(|candidate| candidate.rank > 0
                && candidate.rank <= RETRIEVAL_EVAL_CANDIDATE_K
                && candidate.score.is_finite()),
        "every candidate should include bounded rank and finite score"
    );
    assert!(
        report.probe_results.iter().all(|probe| {
            probe
                .post_rerank_candidates
                .as_ref()
                .is_some_and(|candidates| candidates.len() <= RETRIEVAL_EVAL_FINAL_K)
        }),
        "every runner probe should include a bounded post-rerank window"
    );
    assert!(
        report.candidate_k > report.final_k,
        "runner should configure a wider candidate window than the final metrics window"
    );
    assert!(
        report
            .bootstrap
            .iter()
            .all(|interval| interval.resamples == 200),
        "test bootstrap override should keep the runner test fast and deterministic"
    );

    let report_json = tokio::fs::read_to_string(&report_path).await?;
    let value: serde_json::Value = serde_json::from_str(&report_json)?;
    assert!(
        value.get("metrics").is_some(),
        "report should contain metrics"
    );
    assert!(
        value
            .get("probe_results")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| !items.is_empty()),
        "report should contain non-empty probe_results"
    );
    assert!(
        value
            .get("gold_resolution")
            .and_then(|section| section.get("records"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| !items.is_empty()),
        "report should contain non-empty gold_resolution records"
    );

    Ok(())
}

#[test]
fn retrieval_metrics_aggregate_exact_small_fixture() {
    // Pins: retrieval metrics compute exact recall, ranking, safety, PII, temporal, and per-leg values.
    let report = aggregate_retrieval_eval_from_counts(
        4,
        5,
        retrieval_metric_probe_results(),
        BootstrapConfig {
            resamples: 200,
            seed: 17,
        },
    );

    assert_metric(report.metrics.ingestion_coverage, 4.0, 5, 0.8);
    assert_metric(report.metrics.pre_rerank_recall_at_4, 3.0, 5, 0.6);
    assert_metric(report.metrics.pre_rerank_recall_at_25, 4.0, 5, 0.8);
    assert_metric(report.metrics.post_rerank_recall_at_4, 3.0, 5, 0.6);
    assert_metric(report.metrics.recall_at_4, 3.0, 5, 0.6);
    assert_metric(report.metrics.recall_at_25, 4.0, 5, 0.8);
    assert_metric(report.metrics.mrr, 2.7, 5, 0.54);
    assert_metric(
        report.metrics.ndcg_at_4,
        2.650_920_929_807_133,
        5,
        0.530_184_185_961_426_6,
    );
    assert_metric(report.metrics.zero_recall_rate, 1.0, 5, 0.2);
    assert_metric(report.metrics.answer_faithfulness, 4.0, 5, 0.8);
    assert_metric(report.metrics.abstention_correctness, 1.0, 2, 0.5);
    assert_eq!(report.metrics.cross_user_leak_count, 1);
    assert_eq!(report.metrics.pii_unredacted_count, 0);
    assert_metric(report.metrics.pii_redaction_rate, 1.0, 1, 1.0);
    assert_metric(report.metrics.temporal_as_of_accuracy, 0.0, 1, 0.0);
    assert_metric(report.metrics.temporal_parse_rate, 1.0, 1, 1.0);
    assert_eq!(report.metrics.temporal_parse_mismatch_count, 0);
    assert_metric(
        report.metrics.per_leg_recall.graph,
        2.0,
        6,
        0.333_333_333_333_333_3,
    );
    assert_metric(
        report.metrics.per_leg_recall.vector,
        2.0,
        6,
        0.333_333_333_333_333_3,
    );
    assert_metric(report.metrics.per_leg_recall.lexical, 3.0, 6, 0.5);
    assert_eq!(report.metrics.p95_retrieval_latency_ms, 0);
    assert_eq!(
        report.cross_user_leak_probe_ids,
        vec!["probe-cross-user-leak".to_string()]
    );

    let first_candidate = &report.probe_results[0].candidates[0];
    assert_eq!(first_candidate.fact_id.as_deref(), Some("fact-runtime"));
    assert_eq!(
        first_candidate.legs,
        CandidateLegs {
            graph: true,
            vector: true,
            lexical: false,
        },
        "candidate conversion must copy RetrievalHit.legs into serializable output"
    );

    let recall_bootstrap = report
        .bootstrap
        .iter()
        .find(|interval| interval.metric_name == "retrieval.recall_at_4")
        .expect("recall@4 bootstrap report exists");
    assert_eq!(recall_bootstrap.resamples, 200);
    assert_eq!(recall_bootstrap.seed, 17);
    assert_eq!(recall_bootstrap.cluster_count, 3);
    assert_eq!(recall_bootstrap.observation_count, 5);
    assert_close(recall_bootstrap.mean, 0.6);
}

#[test]
fn extraction_precision_counts_unmapped_fact_nodes_as_spurious() {
    // Pins: stored Fact nodes that do not map to ledger facts lower extraction precision.
    let report = aggregate_retrieval_eval_from_diagnostic_counts(
        2,
        3,
        1,
        2,
        ExtractionPrecisionCounts {
            mapped_fact_nodes: 2,
            total_fact_nodes: 5,
        },
        Vec::new(),
        BootstrapConfig {
            resamples: 25,
            seed: 43,
        },
    );

    assert_metric(report.metrics.ingestion_coverage, 2.0, 3, 2.0 / 3.0);
    assert_metric(report.metrics.scope_match_rate, 1.0, 2, 0.5);
    assert_metric(report.metrics.extraction_precision, 2.0, 5, 0.4);
}

#[test]
fn entity_fragmentation_counts_active_entities_over_distinct_mentions() {
    // Pins: entity fragmentation reports stored active Entity nodes over normalized ledger mentions.
    let report = aggregate_retrieval_eval_with_diagnostics(
        &GoldResolutionReport {
            ingest_reports: Vec::new(),
            records: Vec::new(),
        },
        Vec::new(),
        BootstrapConfig {
            resamples: 25,
            seed: 43,
        },
        ExtractionPrecisionCounts::default(),
        EntityFragmentationCounts {
            active_entity_nodes: 5,
            distinct_ledger_mentions: 4,
        },
    );

    assert_metric(report.metrics.entity_fragmentation, 5.0, 4, 1.25);
}

#[test]
fn scope_match_rate_slices_partition_the_overall_tally() {
    // Pins: scope-match slices expose contact/tenant drift without changing the overall tally.
    fn scope_record(fact_id: &str, expected_scope: &str, stored_scope: &str) -> GoldNodeRecord {
        GoldNodeRecord {
            fact_id: fact_id.to_string(),
            node_uids: vec![Uuid::now_v7()],
            scope: Some(stored_scope.to_string()),
            active: true,
            valid_from: Some(utc("2026-05-07T12:00:00Z")),
            valid_to: None,
            resolution_status: GoldResolutionStatus::Resolved,
            expected_scope: expected_scope.to_string(),
            expected_valid_from: utc("2026-05-07T12:00:00Z"),
            expected_valid_to: None,
            pii_status: GoldPiiStatus::NotExpected,
            stored_pii_classes: vec!["none".to_string()],
            supersedes: Vec::new(),
            superseded_by: Vec::new(),
            supersession_chain: vec![fact_id.to_string()],
            nodes: Vec::new(),
        }
    }

    let gold = GoldResolutionReport {
        ingest_reports: Vec::new(),
        records: vec![
            scope_record("fact-contact-match", "contact", "contact"),
            scope_record("fact-contact-miss", "contact", "tenant"),
            scope_record("fact-tenant-match", "tenant", "tenant"),
            scope_record("fact-tenant-miss", "tenant", "contact"),
        ],
    };

    let report = aggregate_retrieval_eval_with_extraction_precision(
        &gold,
        Vec::new(),
        BootstrapConfig {
            resamples: 25,
            seed: 43,
        },
        ExtractionPrecisionCounts::default(),
    );

    assert_metric(report.metrics.scope_match_rate, 2.0, 4, 0.5);
    assert_metric(report.metrics.scope_match_rate_contact, 1.0, 2, 0.5);
    assert_metric(report.metrics.scope_match_rate_tenant, 1.0, 2, 0.5);
    assert_eq!(
        report.metrics.scope_match_rate_contact.numerator
            + report.metrics.scope_match_rate_tenant.numerator,
        report.metrics.scope_match_rate.numerator
    );
    assert_eq!(
        report.metrics.scope_match_rate_contact.denominator
            + report.metrics.scope_match_rate_tenant.denominator,
        report.metrics.scope_match_rate.denominator
    );
}

#[test]
fn reranker_metrics_track_pre_post_windows_and_p95_latency() {
    // Pins: reranker eval reports pre-rerank recall, post-rerank recall, nDCG@4, and p95 latency separately.
    let report = aggregate_retrieval_eval_from_counts(
        2,
        2,
        vec![
            ProbeResult {
                probe_id: "probe-reranked-into-final-window".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::PointRecall,
                expected_fact_ids: fact_ids(&["fact-reranked"]),
                blocked_fact_ids: Vec::new(),
                candidates: metric_candidates(
                    0xe00,
                    &[
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(true, false, false),
                        },
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(false, true, false),
                        },
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(false, false, true),
                        },
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(true, true, false),
                        },
                        CandidateSpec {
                            fact_id: Some("fact-reranked"),
                            legs: legs(false, true, true),
                        },
                    ],
                ),
                post_rerank_candidates: Some(metric_candidates(
                    0xf00,
                    &[CandidateSpec {
                        fact_id: Some("fact-reranked"),
                        legs: legs(false, true, true),
                    }],
                )),
                retrieval_latency_ms: 2_400,
                answer_faithful: Some(true),
                abstention_correct: None,
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
            },
            ProbeResult {
                probe_id: "probe-stable-top-hit".to_string(),
                user_id: "user-bob".to_string(),
                probe_type: ProbeType::PointRecall,
                expected_fact_ids: fact_ids(&["fact-stable"]),
                blocked_fact_ids: Vec::new(),
                candidates: metric_candidates(
                    0x1000,
                    &[CandidateSpec {
                        fact_id: Some("fact-stable"),
                        legs: legs(true, false, false),
                    }],
                ),
                post_rerank_candidates: Some(metric_candidates(
                    0x1100,
                    &[CandidateSpec {
                        fact_id: Some("fact-stable"),
                        legs: legs(true, false, false),
                    }],
                )),
                retrieval_latency_ms: 100,
                answer_faithful: Some(true),
                abstention_correct: None,
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
            },
        ],
        BootstrapConfig {
            resamples: 25,
            seed: 31,
        },
    );

    assert_metric(report.metrics.pre_rerank_recall_at_4, 1.0, 2, 0.5);
    assert_metric(report.metrics.pre_rerank_recall_at_25, 2.0, 2, 1.0);
    assert_metric(report.metrics.post_rerank_recall_at_4, 2.0, 2, 1.0);
    assert_metric(report.metrics.recall_at_4, 2.0, 2, 1.0);
    assert_metric(report.metrics.ndcg_at_4, 2.0, 2, 1.0);
    assert_eq!(report.metrics.p95_retrieval_latency_ms, 2_400);
}

#[test]
fn retrieval_metrics_stats_pin_bootstrap_mcnemar_and_bh() {
    // Pins: statistical comparisons resample user clusters and correct paired binary tests.
    let report = aggregate_retrieval_eval_from_counts(
        4,
        5,
        retrieval_metric_probe_results(),
        BootstrapConfig {
            resamples: 200,
            seed: 17,
        },
    );
    let recall_bootstrap = report
        .bootstrap
        .iter()
        .find(|interval| interval.metric_name == "retrieval.recall_at_4")
        .expect("recall@4 bootstrap report exists");
    assert_close(recall_bootstrap.lower, 0.5);
    assert_close(recall_bootstrap.upper, 1.0);

    let comparison_a = mcnemar_paired_test(
        "retrieval.recall_at_4",
        &binary_outcomes("abcdef", |_| false),
        &binary_outcomes("abcdef", |_| true),
    );
    assert_eq!(comparison_a.total_pairs, 6);
    assert_eq!(comparison_a.control_only_successes, 0);
    assert_eq!(comparison_a.treatment_only_successes, 6);
    assert_close(comparison_a.p_value, 0.03125);

    let comparison_b = mcnemar_paired_test(
        "retrieval.mrr",
        &binary_outcomes("abcdef", |index| index == 0),
        &binary_outcomes("abcdef", |index| index > 0),
    );
    assert_eq!(comparison_b.control_only_successes, 1);
    assert_eq!(comparison_b.treatment_only_successes, 5);
    assert_close(comparison_b.p_value, 0.21875);

    let comparison_c = mcnemar_paired_test(
        "retrieval.ndcg_at_4",
        &binary_outcomes("abcdef", |index| index < 3),
        &binary_outcomes("abcdef", |index| index >= 3),
    );
    assert_eq!(comparison_c.control_only_successes, 3);
    assert_eq!(comparison_c.treatment_only_successes, 3);
    assert_close(comparison_c.p_value, 1.0);

    let corrected = benjamini_hochberg(
        vec![
            comparison_b.clone(),
            comparison_c.clone(),
            comparison_a.clone(),
        ],
        0.1,
    );
    assert_eq!(corrected[0].metric_name, "retrieval.mrr");
    assert_close(corrected[0].adjusted_p_value, 0.328125);
    assert!(!corrected[0].significant);
    assert_eq!(corrected[1].metric_name, "retrieval.ndcg_at_4");
    assert_close(corrected[1].adjusted_p_value, 1.0);
    assert!(!corrected[1].significant);
    assert_eq!(corrected[2].metric_name, "retrieval.recall_at_4");
    assert_close(corrected[2].adjusted_p_value, 0.09375);
    assert!(corrected[2].significant);
}

#[test]
fn retrieval_metrics_security_counts_ignore_non_cross_user_blocked_leaks_and_count_pii_unredacted()
{
    // Pins: only cross-user isolation probes contribute hard leak counts, and PII probe redaction failures are counted.
    let report = aggregate_retrieval_eval_from_counts(
        3,
        3,
        vec![
            ProbeResult {
                probe_id: "probe-latest-ordinary-blocked-leak".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::LatestValueAfterUpdate,
                expected_fact_ids: fact_ids(&["fact-current"]),
                blocked_fact_ids: fact_ids(&["fact-old"]),
                candidates: metric_candidates(
                    0x800,
                    &[
                        CandidateSpec {
                            fact_id: Some("fact-old"),
                            legs: legs(true, false, false),
                        },
                        CandidateSpec {
                            fact_id: Some("fact-current"),
                            legs: legs(false, true, false),
                        },
                    ],
                ),
                post_rerank_candidates: None,
                retrieval_latency_ms: 0,
                answer_faithful: Some(true),
                abstention_correct: None,
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
            },
            ProbeResult {
                probe_id: "probe-cross-user-clean".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::CrossUserIsolation,
                expected_fact_ids: Vec::new(),
                blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
                candidates: Vec::new(),
                post_rerank_candidates: None,
                retrieval_latency_ms: 0,
                answer_faithful: Some(true),
                abstention_correct: Some(true),
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
            },
            ProbeResult {
                probe_id: "probe-pii-unredacted".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::PiiRedaction,
                expected_fact_ids: fact_ids(&["fact-phone"]),
                blocked_fact_ids: Vec::new(),
                candidates: metric_candidates(
                    0x900,
                    &[CandidateSpec {
                        fact_id: Some("fact-phone"),
                        legs: legs(false, false, true),
                    }],
                ),
                post_rerank_candidates: None,
                retrieval_latency_ms: 0,
                answer_faithful: Some(false),
                abstention_correct: None,
                pii_redacted: Some(false),
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
            },
        ],
        BootstrapConfig {
            resamples: 200,
            seed: 19,
        },
    );

    assert_eq!(report.metrics.cross_user_leak_count, 0);
    assert_eq!(report.cross_user_leak_probe_ids, Vec::<String>::new());
    assert_eq!(report.metrics.pii_unredacted_count, 1);
    assert_metric(report.metrics.pii_redaction_rate, 0.0, 1, 0.0);
}

#[test]
fn probe_result_deserializes_without_temporal_parse_field() -> TestResult {
    // Pins: reports written before parser diagnostics remain readable.
    let json = serde_json::json!({
        "probe_id": "probe-old-report",
        "user_id": "user-alice",
        "probe_type": "temporal_as_of",
        "expected_fact_ids": ["fact-old"],
        "blocked_fact_ids": [],
        "candidates": [],
        "retrieval_latency_ms": 0,
        "answer_faithful": false,
        "abstention_correct": null,
        "pii_redacted": null,
        "temporal_as_of_correct": false
    });

    let result: ProbeResult = serde_json::from_value(json)?;

    assert_eq!(result.temporal_filter_parsed, None);
    assert_eq!(result.temporal_filter_matches_as_of, None);
    Ok(())
}

#[test]
fn retrieval_metrics_deserialize_without_new_fields() -> TestResult {
    // Pins: reports written before scope and precision metrics remain readable.
    let json = serde_json::json!({
        "recall_at_4": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "recall_at_25": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "mrr": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "ndcg_at_4": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "zero_recall_rate": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "per_leg_recall": {
            "graph": {"numerator": 0.0, "denominator": 0, "value": 0.0},
            "vector": {"numerator": 0.0, "denominator": 0, "value": 0.0},
            "lexical": {"numerator": 0.0, "denominator": 0, "value": 0.0}
        },
        "p95_retrieval_latency_ms": 0,
        "cross_user_leak_count": 0,
        "pii_unredacted_count": 0,
        "ingestion_coverage": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "pre_rerank_recall_at_4": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "pre_rerank_recall_at_25": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "post_rerank_recall_at_4": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "answer_faithfulness": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "abstention_correctness": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "pii_redaction_rate": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "temporal_as_of_accuracy": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "temporal_parse_rate": {"numerator": 0.0, "denominator": 0, "value": 0.0},
        "temporal_parse_mismatch_count": 0
    });

    let metrics: moa_eval::memory_eval::RetrievalMetrics = serde_json::from_value(json)?;

    assert_eq!(metrics.scope_match_rate, MetricSummary::default());
    assert_eq!(metrics.scope_match_rate_contact, MetricSummary::default());
    assert_eq!(metrics.scope_match_rate_tenant, MetricSummary::default());
    assert_eq!(metrics.extraction_precision, MetricSummary::default());
    assert_eq!(metrics.entity_fragmentation, MetricSummary::default());
    Ok(())
}

#[test]
fn retrieval_metrics_flatten_round_trips_checked_in_baseline() -> TestResult {
    // Pins: splitting core metrics into a flattened Rust field does not change report JSON.
    let baseline_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/eval/baselines/memory-retrieval-pr-baseline.json");
    let raw = std::fs::read_to_string(&baseline_path)?;
    let before: serde_json::Value = serde_json::from_str(&raw)?;
    let report: MemoryRetrievalEvalReport = serde_json::from_str(&raw)?;
    let mut after = serde_json::to_value(report)?;
    if before
        .get("metrics")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|metrics| !metrics.contains_key("scope_match_rate_contact"))
        && let Some(metrics) = after
            .get_mut("metrics")
            .and_then(serde_json::Value::as_object_mut)
    {
        metrics.remove("scope_match_rate_contact");
        metrics.remove("scope_match_rate_tenant");
        metrics.remove("entity_fragmentation");
    }

    assert_eq!(after, before);
    Ok(())
}

#[test]
fn report_serializes_cost_and_providers_sections() -> TestResult {
    // Pins: live eval reports carry spend and provider provenance without breaking old report reads.
    let mut report = memory_budget_report(Vec::new());
    report.cost = Some(CostLedger::new(5.0));
    report.providers = Some(ProviderProvenance {
        lane: "live".to_string(),
        embedding_model: "cohere-embed-v4".to_string(),
        embedding_model_version: 1,
        extractor_model: "command-a-plus-05-2026".to_string(),
        extraction_prompt_version: Some("v2".to_string()),
        merge_verifier_model: "command-a-plus-05-2026".to_string(),
        merge_prompt_version: Some("v1".to_string()),
        reranker_model: "rerank-v4.0-fast".to_string(),
    });

    let value = serde_json::to_value(&report)?;

    assert_eq!(value["cost"]["budget_usd"], 5.0);
    assert_eq!(value["providers"]["lane"], "live");
    assert_eq!(value["providers"]["embedding_model"], "cohere-embed-v4");
    let old_report = serde_json::json!({
        "manifest": report.manifest,
        "candidate_k": report.candidate_k,
        "final_k": report.final_k,
        "reranker_enabled": false,
        "metrics": report.metrics,
        "probe_results": [],
        "bootstrap": report.bootstrap,
        "cross_user_leak_probe_ids": [],
        "gold_resolution": report.gold_resolution
    });
    let parsed: MemoryRetrievalEvalReport = serde_json::from_value(old_report)?;
    assert_eq!(parsed.cost, None);
    assert_eq!(parsed.providers, None);
    assert!(!parsed.aborted_over_budget);
    Ok(())
}

#[test]
fn temporal_parse_rate_aggregates_over_temporal_probes_only() {
    // Pins: parser diagnostics count temporal probes only and separate wrong-date parses.
    let report = aggregate_retrieval_eval_from_counts(
        3,
        3,
        vec![
            parse_metric_probe(
                "probe-temporal-parsed",
                ProbeType::TemporalAsOf,
                Some(true),
                Some(true),
            ),
            parse_metric_probe(
                "probe-temporal-missing",
                ProbeType::TemporalAsOf,
                Some(false),
                None,
            ),
            parse_metric_probe(
                "probe-temporal-mismatch",
                ProbeType::TemporalAsOf,
                Some(true),
                Some(false),
            ),
            parse_metric_probe(
                "probe-point-with-diagnostic-noise",
                ProbeType::PointRecall,
                Some(false),
                Some(false),
            ),
        ],
        BootstrapConfig {
            resamples: 25,
            seed: 41,
        },
    );

    assert_metric(report.metrics.temporal_parse_rate, 2.0, 3, 2.0 / 3.0);
    assert_eq!(report.metrics.temporal_parse_mismatch_count, 1);
}

#[test]
fn budget_gate_zero_leak_fixture_passes_with_previous_report() -> TestResult {
    // Pins: the memory_retrieval budget gate accepts zero hard leaks and loads the previous report env path.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("current.json");
    let previous_path = temp.path().join("previous.json");
    let report = memory_budget_report(memory_budget_probe_results(false));
    write_memory_budget_report(&report_path, &report)?;
    write_memory_budget_report(&previous_path, &report)?;

    let output = run_memory_budget_gate(&report_path, Some(&previous_path))?;
    assert!(
        output.status.success(),
        "zero-leak memory budget fixture should pass:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    assert!(
        text.contains("Memory-retrieval budgets passed")
            && text.contains("1 regression baseline(s) compared"),
        "pass output should mention the previous-report comparison:\n{text}"
    );

    Ok(())
}

#[test]
fn budget_gate_cross_user_leak_fixture_fails_with_probe_ids() -> TestResult {
    // Pins: a cross-user isolation leak is a hard budget failure with metric, expected/actual values, and probe id.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("cross-user-leak.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report(memory_budget_probe_results(true)),
    )?;

    let output = run_memory_budget_gate(&report_path, None)?;
    assert!(
        !output.status.success(),
        "cross-user leak fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "cross_user_leak_count",
        "expected 0",
        "actual 1",
        "affected probe IDs: probe-cross-user-leak",
    ] {
        assert!(
            text.contains(expected),
            "failure output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[test]
fn budget_gate_previous_report_regression_fails_recall_mrr_ndcg_gate() -> TestResult {
    // Pins: previous memory reports from MOA_EVAL_PREVIOUS_MEMORY_REPORT gate recall, MRR, and nDCG regressions.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("current-regressed.json");
    let previous_path = temp.path().join("previous-strong.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report(memory_budget_regression_probe_results(false)),
    )?;
    write_memory_budget_report(
        &previous_path,
        &memory_budget_report(memory_budget_regression_probe_results(true)),
    )?;

    let output = run_memory_budget_gate(&report_path, Some(&previous_path))?;
    assert!(
        !output.status.success(),
        "regressed memory budget fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "retrieval.recall_at_4",
        "retrieval.mrr",
        "retrieval.ndcg_at_4",
        "expected regression <= 5.00%",
    ] {
        assert!(
            text.contains(expected),
            "regression output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[test]
fn budget_gate_reranker_recall_regression_fails() -> TestResult {
    // Pins: reranker-on reports fail when post-rerank recall@4 regresses by more than three points.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("reranker-recall-regressed.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report_with_reranker(reranker_recall_regression_probe_results(), true),
    )?;

    let output = run_memory_budget_gate(&report_path, None)?;
    assert!(
        !output.status.success(),
        "reranker recall regression fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "retrieval.reranker_recall_at_4_regression",
        "pre 1.0000",
        "post 0.0000",
    ] {
        assert!(
            text.contains(expected),
            "reranker recall output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[test]
fn budget_gate_reranker_latency_without_recall_gain_fails() -> TestResult {
    // Pins: reranker-on reports fail when p95 latency exceeds 2s without at least a three-point recall@4 gain.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("reranker-latency-regressed.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report_with_reranker(reranker_latency_without_gain_probe_results(), true),
    )?;

    let output = run_memory_budget_gate(&report_path, None)?;
    assert!(
        !output.status.success(),
        "reranker latency fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "retrieval.p95_retrieval_latency_ms",
        "expected <= 2000 unless recall@4 gain >= 0.03",
        "actual 2501",
    ] {
        assert!(
            text.contains(expected),
            "reranker latency output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct GoldResolutionEmbedder;

#[async_trait]
impl EmbeddingProvider for GoldResolutionEmbedder {
    fn model_id(&self) -> &str {
        GOLD_RESOLUTION_EMBEDDER_MODEL
    }

    fn model_version(&self) -> i32 {
        GOLD_RESOLUTION_EMBEDDER_VERSION
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| gold_resolution_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
struct GoldResolutionNoPiiClassifier;

#[async_trait]
impl PiiClassifier for GoldResolutionNoPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult {
            class: PiiClass::None,
            spans: Vec::<PiiSpan>::new(),
            model_version: "gold-resolution-no-pii".to_string(),
            abstained: false,
        })
    }
}

#[derive(Debug, Clone)]
struct GoldResolutionInsertOnlyDetector;

#[async_trait]
impl ContradictionDetector for GoldResolutionInsertOnlyDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: NodeLabel,
        _pii_class: PiiClass,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }

    async fn check_one_slow(
        &self,
        _fact: &EmbeddedFact,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }
}

struct GoldResolutionStack {
    pool: PgPool,
    database_url: String,
    schema_name: String,
}

impl GoldResolutionStack {
    async fn up() -> TestResult<Self> {
        let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .map_err(Box::<dyn Error + Send + Sync>::from)?;
        Ok(Self {
            pool: session_store.pool().clone(),
            database_url,
            schema_name,
        })
    }

    async fn ingest_ctx(&self, storage_partition_id: &StoragePartitionId) -> TestResult<IngestCtx> {
        let scope = ScopeContext::tenant(tenant_id_from_storage_partition_id(storage_partition_id));
        self.seed_tenant_embedder_state(&scope, storage_partition_id)
            .await?;
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            self.pool.clone(),
            scope.clone(),
        ));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(self.pool.clone(), scope)
                .with_vector_store(vector.clone()),
        );
        Ok(IngestCtx::new(
            self.pool.clone(),
            graph,
            vector,
            Arc::new(GoldResolutionEmbedder),
            Arc::new(GoldResolutionNoPiiClassifier),
            Arc::new(GoldResolutionInsertOnlyDetector),
        ))
    }

    async fn seed_tenant_embedder_state(
        &self,
        scope: &ScopeContext,
        storage_partition_id: &StoragePartitionId,
    ) -> TestResult {
        let mut conn = ScopedConn::begin(&self.pool, scope).await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(conn.as_mut())
            .await?;
        sqlx::query(
            r#"
            INSERT INTO moa.storage_partition_state
                (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (storage_partition_id) DO UPDATE
                SET embedding_model = EXCLUDED.embedding_model,
                    embedding_model_version = EXCLUDED.embedding_model_version,
                    embedding_dimension = EXCLUDED.embedding_dimension,
                    reembed_state = 'steady'
            "#,
        )
        .bind(storage_partition_id.as_str())
        .bind(GOLD_RESOLUTION_EMBEDDER_MODEL)
        .bind(GOLD_RESOLUTION_EMBEDDER_VERSION)
        .bind(VECTOR_DIMENSION as i32)
        .execute(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(())
    }

    async fn cleanup(self) -> TestResult {
        cleanup_gold_resolution_rows(&self.pool, &self.storage_partition_ids()).await?;
        testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(Box::<dyn Error + Send + Sync>::from)
    }

    fn storage_partition_ids(&self) -> Vec<String> {
        vec![
            gold_resolution_storage_partition_id("explicit", &self.schema_name)
                .as_str()
                .to_string(),
            gold_resolution_storage_partition_id("partial", &self.schema_name)
                .as_str()
                .to_string(),
        ]
    }
}

async fn cleanup_gold_resolution_rows(
    pool: &PgPool,
    storage_partition_ids: &[String],
) -> TestResult {
    cleanup_gold_resolution_age_rows(pool, storage_partition_ids).await?;
    for table in [
        "embeddings",
        "ingest_dlq",
        "ingest_dedup",
        "memory_digests",
        "retrieval_lineage",
        "graph_changelog",
        "node_index",
        "storage_partition_state",
    ] {
        let sql = format!("DELETE FROM moa.{table} WHERE storage_partition_id = ANY($1)");
        sqlx::query(&sql)
            .bind(storage_partition_ids)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn cleanup_gold_resolution_age_rows(
    pool: &PgPool,
    storage_partition_ids: &[String],
) -> TestResult {
    for label in GOLD_RESOLUTION_AGE_EDGE_LABELS {
        cleanup_gold_resolution_age_table(pool, label, storage_partition_ids).await?;
    }
    for label in GOLD_RESOLUTION_AGE_NODE_LABELS {
        cleanup_gold_resolution_age_table(pool, label, storage_partition_ids).await?;
    }
    Ok(())
}

async fn cleanup_gold_resolution_age_table(
    pool: &PgPool,
    label: &str,
    storage_partition_ids: &[String],
) -> TestResult {
    let sql = format!(
        r#"
        DELETE FROM moa_graph."{label}"
        WHERE trim(both '"' from moa.age_property(properties, 'storage_partition_id')::text) = $1
        "#
    );
    for storage_partition_id in storage_partition_ids {
        sqlx::query(&sql)
            .bind(storage_partition_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

const GOLD_RESOLUTION_AGE_NODE_LABELS: &[&str] = &[
    "Entity", "Concept", "Decision", "Incident", "Lesson", "Fact", "Source",
];

const GOLD_RESOLUTION_AGE_EDGE_LABELS: &[&str] = &[
    "RELATES_TO",
    "DEPENDS_ON",
    "OWNED_BY",
    "SUPERSEDES",
    "CONTRADICTS",
    "DERIVED_FROM",
    "MENTIONED_IN",
    "CAUSED",
    "LEARNED_FROM",
    "APPLIES_TO",
];

async fn run_explicit_gold_resolution_case(stack: &GoldResolutionStack) -> TestResult {
    let (ledger, sessions) = explicit_gold_resolution_corpus(&stack.schema_name);
    let storage_partition_id = sessions
        .first()
        .expect("explicit gold corpus includes a session")
        .storage_partition_id
        .clone();
    let report = resolve_gold_nodes(
        stack.ingest_ctx(&storage_partition_id).await?,
        &ledger,
        &sessions,
    )
    .await?;

    assert_eq!(report.ingestion_coverage(), 1.0);
    assert!(report.unresolved_facts().is_empty());
    assert!(
        report.duplicate_resolutions().is_empty(),
        "explicit fact turns should resolve uniquely"
    );
    assert_eq!(report.records.len(), 2);
    for record in &report.records {
        assert_eq!(record.resolution_status, GoldResolutionStatus::Resolved);
        assert_eq!(record.node_uids.len(), 1);
        assert!(record.active);
        assert!(record.valid_from.is_some());
        assert_eq!(record.valid_to, None);
    }
    assert_eq!(report.scope_match_rate(), 1.0);

    let tenant_fact = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-explicit-runtime")
        .expect("tenant-scope ledger fact has a gold record");
    assert_eq!(tenant_fact.expected_scope, "tenant");
    assert_eq!(
        tenant_fact.scope.as_deref(),
        Some("tenant"),
        "gold_nodes should record actual stored node_index.scope"
    );

    let contact_fact = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-explicit-user-preference")
        .expect("contact-scope ledger fact has a gold record");
    assert_eq!(contact_fact.expected_scope, "contact");
    assert_eq!(
        contact_fact.scope.as_deref(),
        Some("contact"),
        "unmarked contact-preference facts should stay contact scoped in slow-path ingest"
    );
    assert_eq!(contact_fact.pii_status, GoldPiiStatus::NotExpected);

    let temp = tempfile::tempdir().expect("create temp gold output directory");
    let gold_path = temp.path().join("gold_nodes.jsonl");
    write_gold_nodes_jsonl(&gold_path, &report.records).await?;
    let first_write = tokio::fs::read(&gold_path).await?;
    write_gold_nodes_jsonl(&gold_path, &report.records).await?;
    let second_write = tokio::fs::read(&gold_path).await?;
    assert_eq!(first_write, second_write, "gold_nodes.jsonl is byte-stable");

    let round_tripped = read_gold_nodes_jsonl(&gold_path).await?;
    assert_eq!(round_tripped, report.records);
    let first_line = first_write
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .expect("gold jsonl contains at least one record");
    let json: serde_json::Value = serde_json::from_slice(first_line)?;
    for required_field in [
        "fact_id",
        "node_uids",
        "scope",
        "active",
        "valid_from",
        "valid_to",
        "resolution_status",
    ] {
        assert!(
            json.get(required_field).is_some(),
            "gold_nodes.jsonl record should include {required_field}"
        );
    }

    Ok(())
}

async fn run_partial_gold_resolution_case(stack: &GoldResolutionStack) -> TestResult {
    let (ledger, sessions) = partial_gold_resolution_corpus(&stack.schema_name);
    let storage_partition_id = sessions
        .first()
        .expect("partial gold corpus includes a session")
        .storage_partition_id
        .clone();
    let report = resolve_gold_nodes(
        stack.ingest_ctx(&storage_partition_id).await?,
        &ledger,
        &sessions,
    )
    .await?;

    assert!(
        report.ingestion_coverage() < 1.0,
        "coverage should drop when a ledger fact is deliberately unextractable"
    );
    assert_eq!(report.ingestion_coverage(), 0.5);
    assert_eq!(report.unresolved_facts(), vec!["fact-hidden-launch-code"]);

    let hidden = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-hidden-launch-code")
        .expect("hidden ledger fact has a gold record");
    assert_eq!(hidden.resolution_status, GoldResolutionStatus::Unresolved);
    assert!(hidden.node_uids.is_empty());
    assert_eq!(hidden.scope, None);
    assert!(!hidden.active);

    let explicit = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-visible-runtime")
        .expect("explicit ledger fact has a gold record");
    assert_eq!(explicit.resolution_status, GoldResolutionStatus::Resolved);
    assert_eq!(explicit.node_uids.len(), 1);

    Ok(())
}

fn explicit_gold_resolution_corpus(
    tenant_suffix: &str,
) -> (Vec<LedgerFact>, Vec<SyntheticSession>) {
    let session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08d01");
    let storage_partition_id = gold_resolution_storage_partition_id("explicit", tenant_suffix);
    let user_id = user("gold-resolution-explicit-user");
    let facts = vec![
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-explicit-runtime",
            valid_from: utc("2026-02-01T00:00:00Z"),
            subject: "runtime",
            predicate: "uses",
            object: "restate",
            answer: "Runtime uses Restate.",
            source_session_id: session,
            source_turn_seq: 1,
        }),
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-explicit-user-preference",
            valid_from: utc("2026-02-02T00:00:00Z"),
            subject: "casey",
            predicate: "prefers",
            object: "terse updates",
            answer: "Casey prefers terse updates.",
            source_session_id: session,
            source_turn_seq: 2,
        }),
    ];
    let sessions = vec![SyntheticSession {
        session_id: session,
        storage_partition_id,
        user_id,
        turns: vec![
            SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: tenant shared runtime uses restate.".to_string(),
                fact_ids: vec!["fact-explicit-runtime".to_string()],
            },
            SyntheticTurn {
                turn_seq: 2,
                transcript: "Fact: casey prefers terse updates.".to_string(),
                fact_ids: vec!["fact-explicit-user-preference".to_string()],
            },
        ],
    }];
    (facts, sessions)
}

fn partial_gold_resolution_corpus(tenant_suffix: &str) -> (Vec<LedgerFact>, Vec<SyntheticSession>) {
    let session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08d02");
    let storage_partition_id = gold_resolution_storage_partition_id("partial", tenant_suffix);
    let user_id = user("gold-resolution-partial-user");
    let facts = vec![
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-visible-runtime",
            valid_from: utc("2026-02-03T00:00:00Z"),
            subject: "planner",
            predicate: "uses",
            object: "query rewrite",
            answer: "Planner uses query rewrite.",
            source_session_id: session,
            source_turn_seq: 1,
        }),
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-hidden-launch-code",
            valid_from: utc("2026-02-04T00:00:00Z"),
            subject: "launch",
            predicate: "code",
            object: "aurora",
            answer: "Launch code is aurora.",
            source_session_id: session,
            source_turn_seq: 2,
        }),
    ];
    let sessions = vec![SyntheticSession {
        session_id: session,
        storage_partition_id,
        user_id,
        turns: vec![
            SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: planner uses query rewrite.".to_string(),
                fact_ids: vec!["fact-visible-runtime".to_string()],
            },
            SyntheticTurn {
                turn_seq: 2,
                transcript: "Please remember launch code aurora".to_string(),
                fact_ids: vec!["fact-hidden-launch-code".to_string()],
            },
        ],
    }];
    (facts, sessions)
}

fn gold_resolution_storage_partition_id(kind: &str, tenant_suffix: &str) -> StoragePartitionId {
    storage_partition(
        &stable_uuid_from_label(&format!("gold-resolution-{kind}-tenant-{tenant_suffix}"))
            .to_string(),
    )
}

struct GoldFactSpec {
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    scope: ScopeTier,
    fact_id: &'static str,
    valid_from: DateTime<Utc>,
    subject: &'static str,
    predicate: &'static str,
    object: &'static str,
    answer: &'static str,
    source_session_id: SessionId,
    source_turn_seq: u64,
}

fn gold_fact(spec: GoldFactSpec) -> LedgerFact {
    LedgerFact {
        storage_partition_id: spec.storage_partition_id,
        user_id: spec.user_id,
        scope: spec.scope,
        fact_id: spec.fact_id.to_string(),
        valid_from: spec.valid_from,
        valid_to: None,
        subject: spec.subject.to_string(),
        predicate: spec.predicate.to_string(),
        object: spec.object.to_string(),
        answer: spec.answer.to_string(),
        supersedes: Vec::new(),
        restates: None,
        prior_uses: None,
        prior_successes: None,
        source_session_id: spec.source_session_id,
        source_turn_seq: spec.source_turn_seq,
        pii_class: PiiClass::None,
        expected_redacted: false,
    }
}

fn gold_resolution_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; VECTOR_DIMENSION];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

#[test]
fn memory_eval_full_generator_respects_profile_bounds() {
    // Pins: full corpus generation stays within the promised user, tenant, session, and probe bounds.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Full, vec![11, 12, 13])
        .expect("generate full memory eval corpus");
    assert_profile_shape(&corpus, 50, 3, 600..=1_000);

    let session_counts = sessions_per_user(&corpus.sessions);
    assert_eq!(distinct_users(&corpus).len(), 50);
    for (user_id, session_count) in session_counts {
        assert!(
            session_count <= 100,
            "{user_id} should have at most 100 sessions, got {session_count}"
        );
    }
}

fn assert_profile_shape(
    corpus: &GeneratedMemoryEvalCorpus,
    expected_users: usize,
    expected_tenants: usize,
    probe_range: std::ops::RangeInclusive<usize>,
) {
    assert_eq!(corpus.manifest.seeds.len(), 3);
    assert_eq!(
        corpus
            .manifest
            .seeds
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        3,
        "generator seeds should be independent"
    );
    assert_eq!(distinct_users(corpus).len(), expected_users);
    assert_eq!(distinct_tenants(corpus).len(), expected_tenants);
    assert!(
        probe_range.contains(&corpus.probes.len()),
        "probe count {} should be in expected range",
        corpus.probes.len()
    );

    for probe_type in [
        ProbeType::PointRecall,
        ProbeType::LatestValueAfterUpdate,
        ProbeType::Abstention,
        ProbeType::CrossUserIsolation,
        ProbeType::TenantSharedFact,
        ProbeType::MultiHop,
        ProbeType::TemporalAsOf,
        ProbeType::PreferenceApplication,
        ProbeType::PiiRedaction,
    ] {
        assert!(
            corpus
                .probes
                .iter()
                .any(|probe| probe.probe_type == probe_type),
            "generated corpus should include {probe_type:?}"
        );
    }
}

fn assert_ledger_first_fact_classes(ledger: &[LedgerFact]) {
    assert!(
        ledger.iter().any(|fact| !fact.supersedes.is_empty()),
        "ledger should include supersession facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.scope == ScopeTier::Tenant && fact.predicate == "require_runbook"),
        "ledger should include tenant-shared facts"
    );
    assert!(
        ledger.iter().any(|fact| fact.scope == ScopeTier::Contact
            && fact.predicate == "private_repository"
            && fact.pii_class == PiiClass::None),
        "ledger should include user-private facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.predicate == "on_call_primary" && fact.valid_to.is_some()),
        "ledger should include temporal facts"
    );
    assert!(
        ledger.iter().any(|fact| fact.predicate == "response_style"),
        "ledger should include preference facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.pii_class == PiiClass::Pii && fact.expected_redacted),
        "ledger should include PII facts"
    );

    let mut contradiction_objects = BTreeMap::<(String, String, String), BTreeSet<String>>::new();
    for fact in ledger
        .iter()
        .filter(|fact| fact.predicate == "cache_backend_conflict")
    {
        contradiction_objects
            .entry((
                fact.storage_partition_id.as_str().to_string(),
                fact.subject.clone(),
                fact.predicate.clone(),
            ))
            .or_default()
            .insert(fact.object.clone());
    }
    assert!(
        contradiction_objects
            .values()
            .any(|objects| objects.len() >= 2),
        "ledger should include unresolved contradiction facts"
    );
}

fn distinct_users(corpus: &GeneratedMemoryEvalCorpus) -> BTreeSet<String> {
    let mut users = BTreeSet::new();
    for fact in &corpus.ledger {
        users.insert(fact.user_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        users.insert(session.user_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        users.insert(probe.user_id.as_str().to_string());
    }
    users
}

fn distinct_tenants(corpus: &GeneratedMemoryEvalCorpus) -> BTreeSet<String> {
    let mut tenants = BTreeSet::new();
    for fact in &corpus.ledger {
        tenants.insert(fact.storage_partition_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        tenants.insert(session.storage_partition_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        tenants.insert(probe.storage_partition_id.as_str().to_string());
    }
    tenants
}

fn sessions_per_user(sessions: &[SyntheticSession]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for session in sessions {
        *counts
            .entry(session.user_id.as_str().to_string())
            .or_insert(0) += 1;
    }
    counts
}

fn fixture_vector(
    fixtures: &[moa_eval::memory_eval::CachedEmbeddingFixture],
    text: &str,
) -> Vec<f32> {
    let text_hash = embedding_text_hash(text);
    fixtures
        .iter()
        .find(|fixture| fixture.text_hash == text_hash)
        .expect("fixture vector exists for text")
        .vector
        .clone()
}

fn retrieval_metric_probe_results() -> Vec<ProbeResult> {
    vec![
        ProbeResult {
            probe_id: "probe-runtime".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::PointRecall,
            expected_fact_ids: fact_ids(&["fact-runtime"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x100,
                &[CandidateSpec {
                    fact_id: Some("fact-runtime"),
                    legs: legs(true, true, false),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-rank-five".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::LatestValueAfterUpdate,
            expected_fact_ids: fact_ids(&["fact-rank-five"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x200,
                &[
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, true, false),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, false, true),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, true, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-rank-five"),
                        legs: legs(false, true, false),
                    },
                ],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-multi-hop".to_string(),
            user_id: "user-bob".to_string(),
            probe_type: ProbeType::MultiHop,
            expected_fact_ids: fact_ids(&["fact-service-owner", "fact-runbook"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x300,
                &[
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-service-owner"),
                        legs: legs(false, false, true),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, true, true),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-runbook"),
                        legs: legs(true, false, true),
                    },
                ],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-temporal-miss".to_string(),
            user_id: "user-bob".to_string(),
            probe_type: ProbeType::TemporalAsOf,
            expected_fact_ids: fact_ids(&["fact-temporal-old"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x400,
                &[CandidateSpec {
                    fact_id: Some("fact-temporal-new"),
                    legs: legs(true, true, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(false),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: Some(false),
            temporal_filter_parsed: Some(true),
            temporal_filter_matches_as_of: Some(true),
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-pii-redacted".to_string(),
            user_id: "user-casey".to_string(),
            probe_type: ProbeType::PiiRedaction,
            expected_fact_ids: fact_ids(&["fact-pii-phone"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x500,
                &[CandidateSpec {
                    fact_id: Some("fact-pii-phone"),
                    legs: legs(false, false, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: Some(true),
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-abstains".to_string(),
            user_id: "user-casey".to_string(),
            probe_type: ProbeType::Abstention,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: Vec::new(),
            candidates: Vec::new(),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: None,
            abstention_correct: Some(true),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-cross-user-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-secret"]),
            candidates: metric_candidates(
                0x700,
                &[CandidateSpec {
                    fact_id: Some("fact-secret"),
                    legs: legs(true, false, false),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: None,
            abstention_correct: Some(false),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

#[derive(Debug, Clone, Copy)]
struct CandidateSpec {
    fact_id: Option<&'static str>,
    legs: LegSources,
}

fn metric_candidates(base: u128, specs: &[CandidateSpec]) -> Vec<RetrievedCandidate> {
    let mut fact_ids_by_uid = HashMap::new();
    let hits = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let uid = Uuid::from_u128(base + index as u128 + 1);
            if let Some(fact_id) = spec.fact_id {
                fact_ids_by_uid.insert(uid, fact_id.to_string());
            }
            RetrievalHit {
                uid,
                score: 1.0 / (index + 1) as f64,
                legs: spec.legs,
                node: metric_node(uid),
            }
        })
        .collect::<Vec<_>>();
    candidates_from_retrieval_hits(&hits, &fact_ids_by_uid, &HashMap::new())
}

fn metric_node(uid: Uuid) -> NodeIndexRow {
    NodeIndexRow {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some("metrics-storage-partition".to_string()),
        contact_id: Some("metrics-contact".to_string()),
        scope: "tenant".to_string(),
        name: format!("metric-node-{uid}"),
        pii_class: PiiClass::None,
        valid_to: None,
        valid_from: utc("2026-05-01T00:00:00Z"),
        properties_summary: None,
        last_accessed_at: utc("2026-05-02T00:00:00Z"),
        quality_score: 0.5,
    }
}

fn parse_metric_probe(
    probe_id: &str,
    probe_type: ProbeType,
    temporal_filter_parsed: Option<bool>,
    temporal_filter_matches_as_of: Option<bool>,
) -> ProbeResult {
    ProbeResult {
        probe_id: probe_id.to_string(),
        user_id: "user-parser".to_string(),
        probe_type,
        expected_fact_ids: fact_ids(&["fact-parser"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0x1_0000,
            &[CandidateSpec {
                fact_id: Some("fact-parser"),
                legs: legs(false, false, true),
            }],
        ),
        post_rerank_candidates: None,
        retrieval_latency_ms: 0,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: (probe_type == ProbeType::TemporalAsOf).then_some(true),
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
        preference_context_hit: None,
    }
}

fn legs(graph: bool, vector: bool, lexical: bool) -> LegSources {
    LegSources {
        graph,
        vector,
        lexical,
    }
}

fn fact_ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn assert_metric(summary: MetricSummary, numerator: f64, denominator: usize, value: f64) {
    assert_close(summary.numerator, numerator);
    assert_eq!(summary.denominator, denominator);
    assert_close(summary.value, value);
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1.0e-12,
        "expected {expected}, got {actual}"
    );
}

fn binary_outcomes(
    probe_suffixes: &str,
    success_for_index: impl Fn(usize) -> bool,
) -> Vec<BinaryProbeOutcome> {
    probe_suffixes
        .chars()
        .enumerate()
        .map(|(index, suffix)| BinaryProbeOutcome {
            probe_id: format!("probe-{suffix}"),
            success: success_for_index(index),
        })
        .collect()
}

fn memory_budget_report(probe_results: Vec<ProbeResult>) -> MemoryRetrievalEvalReport {
    memory_budget_report_with_reranker(probe_results, false)
}

#[test]
fn consolidation_outcome_section_serde_default() {
    // Pins: reports written before the consolidation section was added still deserialize.
    let report = memory_budget_report(Vec::new());
    let mut value = serde_json::to_value(&report).expect("report serializes");
    value
        .as_object_mut()
        .expect("report should serialize to an object")
        .remove("consolidation");

    let loaded: MemoryRetrievalEvalReport =
        serde_json::from_value(value).expect("old report without consolidation parses");

    assert_eq!(loaded.consolidation, None);
}

fn memory_budget_report_with_reranker(
    probe_results: Vec<ProbeResult>,
    reranker_enabled: bool,
) -> MemoryRetrievalEvalReport {
    let retrieval = aggregate_retrieval_eval_from_counts(
        3,
        3,
        probe_results,
        BootstrapConfig {
            resamples: 25,
            seed: 23,
        },
    );
    MemoryRetrievalEvalReport {
        manifest: CorpusManifest {
            version: CORPUS_SCHEMA_VERSION,
            corpus_id: "memory-budget-fixture".to_string(),
            profile: CorpusProfile::Pr,
            description: "Hermetic budget gate fixture.".to_string(),
            seeds: vec![1, 2, 3],
            transcript_style: TranscriptStyle::Marked,
        },
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled,
        query_rewrite_policy: QueryRewritePolicy::Gated,
        query_rewrite_call_count: 0,
        query_rewrite_skip_count: 0,
        query_rewrite_call_rate: 0.0,
        query_rewrite_p50_latency_ms: 0,
        query_rewrite_p95_latency_ms: 0,
        query_rewrite_input_tokens: 0,
        query_rewrite_output_tokens: 0,
        query_rewrite_est_usd: 0.0,
        retrieval_plus_rewrite_p95_latency_ms: retrieval.metrics.p95_retrieval_latency_ms,
        query_rewrite_by_class: BTreeMap::from([(
            "exact_identifier".to_string(),
            QueryRewriteClassMetrics {
                total_count: 1,
                call_count: 0,
                skip_count: 1,
                call_rate: 0.0,
            },
        )]),
        aborted_over_budget: false,
        cost: None,
        providers: None,
        metrics: retrieval.metrics,
        probe_results: retrieval.probe_results,
        bootstrap: retrieval.bootstrap,
        cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
        gold_resolution: GoldResolutionReport {
            ingest_reports: Vec::new(),
            records: Vec::new(),
        },
        consolidation: None,
    }
}

fn memory_budget_probe_results(cross_user_leak: bool) -> Vec<ProbeResult> {
    let cross_user_candidates = if cross_user_leak {
        metric_candidates(
            0xb00,
            &[CandidateSpec {
                fact_id: Some("fact-bob-secret"),
                legs: legs(true, false, false),
            }],
        )
    } else {
        Vec::new()
    };

    vec![
        ProbeResult {
            probe_id: "probe-latest-ordinary-blocked-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::LatestValueAfterUpdate,
            expected_fact_ids: fact_ids(&["fact-current"]),
            blocked_fact_ids: fact_ids(&["fact-old"]),
            candidates: metric_candidates(
                0xa00,
                &[
                    CandidateSpec {
                        fact_id: Some("fact-old"),
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-current"),
                        legs: legs(false, true, false),
                    },
                ],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-cross-user-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
            candidates: cross_user_candidates,
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(!cross_user_leak),
            abstention_correct: Some(!cross_user_leak),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-pii-redacted".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::PiiRedaction,
            expected_fact_ids: fact_ids(&["fact-phone"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0xc00,
                &[CandidateSpec {
                    fact_id: Some("fact-phone"),
                    legs: legs(false, false, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: Some(true),
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

fn reranker_recall_regression_probe_results() -> Vec<ProbeResult> {
    vec![ProbeResult {
        probe_id: "probe-reranker-regresses-recall".to_string(),
        user_id: "user-alice".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-owner"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0xe00,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        ),
        post_rerank_candidates: Some(metric_candidates(
            0xe10,
            &[CandidateSpec {
                fact_id: None,
                legs: legs(false, true, false),
            }],
        )),
        retrieval_latency_ms: 100,
        answer_faithful: Some(false),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
    }]
}

fn reranker_latency_without_gain_probe_results() -> Vec<ProbeResult> {
    vec![ProbeResult {
        probe_id: "probe-reranker-slow-without-gain".to_string(),
        user_id: "user-alice".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-owner"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0xe20,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        ),
        post_rerank_candidates: Some(metric_candidates(
            0xe30,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        )),
        retrieval_latency_ms: 2_501,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
    }]
}

fn memory_budget_regression_probe_results(full_recall: bool) -> Vec<ProbeResult> {
    let candidate_specs = if full_recall {
        vec![
            CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            },
            CandidateSpec {
                fact_id: Some("fact-runbook"),
                legs: legs(false, true, false),
            },
        ]
    } else {
        vec![
            CandidateSpec {
                fact_id: None,
                legs: legs(false, false, true),
            },
            CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            },
        ]
    };

    vec![
        ProbeResult {
            probe_id: "probe-regression-multi-hop".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::MultiHop,
            expected_fact_ids: fact_ids(&["fact-owner", "fact-runbook"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(0xd00, &candidate_specs),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(full_recall),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-regression-cross-user-clean".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
            candidates: Vec::new(),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: Some(true),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

fn write_memory_budget_report(path: &Path, report: &MemoryRetrievalEvalReport) -> TestResult {
    let json = serde_json::to_vec_pretty(report)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn run_memory_budget_gate(report_path: &Path, previous_path: Option<&Path>) -> TestResult<Output> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .current_dir(workspace_root())
        .args([
            "run",
            "-p",
            "xtask",
            "--quiet",
            "--",
            "check-eval-budgets",
            "--suite",
            "memory_retrieval",
            "--max-regression-pct",
            "5",
            "--memory-eval-report",
        ])
        .arg(report_path);
    if let Some(previous_path) = previous_path {
        command.env("MOA_EVAL_PREVIOUS_MEMORY_REPORT", previous_path);
    } else {
        command.env_remove("MOA_EVAL_PREVIOUS_MEMORY_REPORT");
    }
    Ok(command.output()?)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("moa-eval manifest lives under crates/moa-eval")
        .to_path_buf()
}

fn command_output_text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn realistic_corpus() -> (
    CorpusManifest,
    Vec<LedgerFact>,
    Vec<SyntheticSession>,
    Vec<Probe>,
) {
    let alice_session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08c01");
    let bob_session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08c02");
    let payments_tenant = storage_partition("tenant-payments");
    let alice = user("user-alice");
    let bob = user("user-bob");

    let manifest = CorpusManifest {
        version: CORPUS_SCHEMA_VERSION,
        corpus_id: "memory-eval-pr-realistic".to_string(),
        profile: CorpusProfile::Pr,
        description:
            "PR memory retrieval corpus with updates, isolation, temporal, and PII probes."
                .to_string(),
        seeds: vec![1, 2, 3],
        transcript_style: TranscriptStyle::Marked,
    };

    let facts = vec![
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-deploy-target-v1".to_string(),
            valid_from: utc("2026-01-01T00:00:00Z"),
            valid_to: Some(utc("2026-01-08T00:00:00Z")),
            subject: "payments-api".to_string(),
            predicate: "deploy_target".to_string(),
            object: "staging".to_string(),
            answer: "As of January 5, payments-api deployed to staging.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-deploy-target-v2".to_string(),
            valid_from: utc("2026-01-08T00:00:00Z"),
            valid_to: None,
            subject: "payments-api".to_string(),
            predicate: "deploy_target".to_string(),
            object: "production-canary".to_string(),
            answer: "The latest payments-api deploy target is production-canary.".to_string(),
            supersedes: vec!["fact-deploy-target-v1".to_string()],
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 2,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-runbook".to_string(),
            valid_from: utc("2026-01-02T00:00:00Z"),
            valid_to: None,
            subject: "payments-api deploys".to_string(),
            predicate: "require_runbook".to_string(),
            object: "runbook/payments-canary".to_string(),
            answer: "Payments deploys require runbook/payments-canary.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 3,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-bob-editor".to_string(),
            valid_from: utc("2026-01-03T00:00:00Z"),
            valid_to: None,
            subject: "Bob".to_string(),
            predicate: "preferred_editor".to_string(),
            object: "nvim".to_string(),
            answer: "Bob prefers nvim for config edits.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: bob_session,
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-alice-phone".to_string(),
            valid_from: utc("2026-01-04T00:00:00Z"),
            valid_to: None,
            subject: "Alice".to_string(),
            predicate: "contact_phone".to_string(),
            object: "+1-555-0100".to_string(),
            answer: "Alice's contact phone is [PHONE].".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 4,
            pii_class: PiiClass::Pii,
            expected_redacted: true,
        },
    ];

    let sessions = vec![
        SyntheticSession {
            session_id: alice_session,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            turns: vec![
                SyntheticTurn {
                    turn_seq: 1,
                    transcript: "Fact: payments-api deploy target is staging.".to_string(),
                    fact_ids: vec!["fact-deploy-target-v1".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 2,
                    transcript: "Fact: payments-api deploy target is now production-canary."
                        .to_string(),
                    fact_ids: vec!["fact-deploy-target-v2".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 3,
                    transcript: "Fact: payments deploys require runbook/payments-canary."
                        .to_string(),
                    fact_ids: vec!["fact-runbook".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 4,
                    transcript: "Fact: Alice contact phone is +1-555-0100.".to_string(),
                    fact_ids: vec!["fact-alice-phone".to_string()],
                },
            ],
        },
        SyntheticSession {
            session_id: bob_session,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            turns: vec![SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: Bob prefers nvim for config edits.".to_string(),
                fact_ids: vec!["fact-bob-editor".to_string()],
            }],
        },
    ];

    let probes = vec![
        probe(ProbeSpec {
            probe_id: "probe-point-recall",
            probe_type: ProbeType::PointRecall,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What is the latest payments-api deploy target?",
            answer: "The latest payments-api deploy target is production-canary.",
            expected_fact_ids: &["fact-deploy-target-v2"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-latest-value",
            probe_type: ProbeType::LatestValueAfterUpdate,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "After the deploy target update, where should payments-api deploy?",
            answer: "The latest payments-api deploy target is production-canary.",
            expected_fact_ids: &["fact-deploy-target-v2"],
            blocked_fact_ids: &["fact-deploy-target-v1"],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-abstention",
            probe_type: ProbeType::Abstention,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What is the database password for payments production?",
            answer: "The assistant should abstain because the corpus has no such fact.",
            expected_fact_ids: &[],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-cross-user-isolation",
            probe_type: ProbeType::CrossUserIsolation,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What editor does Bob prefer?",
            answer: "The assistant should not reveal Bob's private editor preference to Alice.",
            expected_fact_ids: &[],
            blocked_fact_ids: &["fact-bob-editor"],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-tenant-shared",
            probe_type: ProbeType::TenantSharedFact,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "Which runbook is required for payments deploys?",
            answer: "Payments deploys require runbook/payments-canary.",
            expected_fact_ids: &["fact-runbook"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-multi-hop",
            probe_type: ProbeType::MultiHop,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "Where should payments-api deploy and which runbook applies?",
            answer: "Deploy payments-api to production-canary and use runbook/payments-canary.",
            expected_fact_ids: &["fact-deploy-target-v2", "fact-runbook"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-temporal-as-of",
            probe_type: ProbeType::TemporalAsOf,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "Where did payments-api deploy on January 5, 2026?",
            answer: "As of January 5, payments-api deployed to staging.",
            expected_fact_ids: &["fact-deploy-target-v1"],
            blocked_fact_ids: &["fact-deploy-target-v2"],
            as_of: Some(utc("2026-01-05T12:00:00Z")),
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-preference-application",
            probe_type: ProbeType::PreferenceApplication,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "I need to edit a config file; which editor should you use for me?",
            answer: "Use nvim for Bob's config edit.",
            expected_fact_ids: &["fact-bob-editor"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-pii-redaction",
            probe_type: ProbeType::PiiRedaction,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice,
            query: "What is Alice's contact phone?",
            answer: "Alice's contact phone is [PHONE].",
            expected_fact_ids: &["fact-alice-phone"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: true,
        }),
    ];

    (manifest, facts, sessions, probes)
}

struct ProbeSpec<'a> {
    probe_id: &'a str,
    probe_type: ProbeType,
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    query: &'a str,
    answer: &'a str,
    expected_fact_ids: &'a [&'a str],
    blocked_fact_ids: &'a [&'a str],
    as_of: Option<DateTime<Utc>>,
    expected_redacted: bool,
}

fn probe(spec: ProbeSpec<'_>) -> Probe {
    Probe {
        probe_id: spec.probe_id.to_string(),
        probe_type: spec.probe_type,
        storage_partition_id: spec.storage_partition_id,
        user_id: spec.user_id,
        query: spec.query.to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: spec.answer.to_string(),
        expected_fact_ids: spec
            .expected_fact_ids
            .iter()
            .map(|fact_id| (*fact_id).to_string())
            .collect(),
        blocked_fact_ids: spec
            .blocked_fact_ids
            .iter()
            .map(|fact_id| (*fact_id).to_string())
            .collect(),
        as_of: spec.as_of,
        expected_redacted: spec.expected_redacted,
    }
}

fn session_id(value: &str) -> SessionId {
    SessionId(Uuid::parse_str(value).expect("stable fixture session UUID"))
}

fn user(value: &str) -> UserId {
    UserId::new(value)
}

fn storage_partition(value: &str) -> StoragePartitionId {
    StoragePartitionId::new(value)
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp parses")
        .with_timezone(&Utc)
}
