#[path = "memory_eval_gold_db_memory/external_memory.rs"]
mod external_memory;

#[path = "memory_eval_support/common.rs"]
mod common;
use common::*;
use moa_eval::memory_eval::ProbeType;

include!("memory_eval_support/gold.rs");

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
    // Fail clearly instead of passing vacuously when the db-memory lane runs this without Postgres,
    // matching the sibling `gold_resolution_*` test that requires a live database.
    assert!(
        std::env::var_os("MOA_DATABASE_URL").is_some(),
        "memory_retrieval_eval_runner_writes_report_from_cached_embeddings requires MOA_DATABASE_URL \
         (run via `cargo nextest run -p moa-eval --profile db-memory` with Postgres)"
    );

    let _guard = GOLD_RESOLUTION_TEST_LOCK.lock().await;
    let corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
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
    assert!(report.probe_results.iter().all(|probe| {
        probe.all_expected_found_at_4.is_some() != probe.expected_fact_ids.is_empty()
    }));
    let negative_probes = report
        .probe_results
        .iter()
        .filter(|probe| {
            matches!(
                probe.probe_type,
                ProbeType::Abstention | ProbeType::CrossUserIsolation
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(negative_probes.len(), 21);
    assert!(
        negative_probes
            .iter()
            .all(|probe| probe.forbidden_fact_absent_at_4 == Some(true)),
        "hermetic negative probes should keep every forbidden fact out of the final window"
    );
    assert!(
        negative_probes.iter().any(|probe| {
            !probe
                .post_rerank_candidates
                .as_deref()
                .unwrap_or(&probe.candidates)
                .is_empty()
        }),
        "forbidden-fact absence must allow harmless distractor candidates"
    );
    assert!(
        report
            .probe_results
            .iter()
            .filter(|probe| probe.probe_type == ProbeType::TemporalAsOf)
            .all(|probe| probe.retrieval_temporal_as_of_correct == Some(true))
    );
    assert!(
        report
            .probe_results
            .iter()
            .filter(|probe| probe.probe_type == ProbeType::PiiRedaction)
            .all(|probe| probe.stored_pii_redacted == Some(true))
    );

    let report_json = tokio::fs::read_to_string(&report_path).await?;
    let value: serde_json::Value = serde_json::from_str(&report_json)?;
    assert!(
        value.get("metrics").is_some(),
        "report should contain metrics"
    );
    let metrics = value
        .get("metrics")
        .and_then(serde_json::Value::as_object)
        .expect("report metrics should be an object");
    for removed in ["answer_faithfulness", "abstention_correctness"] {
        assert!(
            !metrics.contains_key(removed),
            "retrieval metrics must not serialize answer-quality field {removed}"
        );
    }
    for observed in [
        "all_expected_found_at_4",
        "forbidden_fact_absent_at_4",
        "retrieval_temporal_as_of_correct",
        "stored_pii_redacted",
    ] {
        assert!(
            metrics.contains_key(observed),
            "retrieval metrics should serialize observed field {observed}"
        );
    }
    assert!(
        value
            .get("probe_results")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| !items.is_empty()),
        "report should contain non-empty probe_results"
    );
    for probe in value["probe_results"]
        .as_array()
        .expect("probe_results should be an array")
    {
        assert!(probe.get("answer").is_none());
        assert!(probe.get("answer_faithful").is_none());
        assert!(probe.get("abstention_correct").is_none());
    }
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

#[tokio::test]
async fn empty_store_pre_retrieval_control_scores_zero_db_memory() -> TestResult {
    // Pins: the memory-retrieval suite's pre-retrieval null, run in its real lane.
    // A live isolated tenant that has ingested nothing holds no Fact node, and the
    // production per-probe scorer returns zero recall in every probe-type slice —
    // so a candidate's recall is attributable to retrieval rather than to cases
    // that were already scoreable.
    assert!(
        std::env::var_os("MOA_DATABASE_URL").is_some(),
        "empty_store_pre_retrieval_control_scores_zero_db_memory requires MOA_DATABASE_URL \
         (run via `cargo nextest run -p moa-eval --profile db-memory` with Postgres)"
    );

    let _guard = GOLD_RESOLUTION_TEST_LOCK.lock().await;
    let corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("generate PR memory eval corpus");
    let stack = GoldResolutionStack::up().await?;
    let result = run_empty_store_control_case(&stack, &corpus.probes).await;
    let cleanup = stack.cleanup().await;
    result?;
    cleanup
}

async fn run_empty_store_control_case(
    stack: &GoldResolutionStack,
    probes: &[moa_eval::memory_eval::Probe],
) -> TestResult {
    let storage_partition_id = gold_resolution_storage_partition_id("explicit", &stack.schema_name);
    // Seeds only embedder state for the partition; it stores no graph content.
    let _ctx = stack.ingest_ctx(&storage_partition_id).await?;
    let runtime_storage_partition_id =
        tenant_id_from_storage_partition_id(&storage_partition_id).to_string();

    let scope = RlsContext::tenant(tenant_id_from_storage_partition_id(&storage_partition_id));
    let mut conn = ScopedConn::begin(&stack.pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await?;
    let fact_nodes = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.node_index \
         WHERE storage_partition_id = $1 AND label = 'Fact'",
    )
    .bind(&runtime_storage_partition_id)
    .fetch_one(conn.as_mut())
    .await?;
    conn.commit().await?;
    assert_eq!(fact_nodes, 0, "a freshly created tenant must hold no facts");

    let scores = moa_eval::controls::memory_retrieval::recall_at_4_by_probe_type(
        &moa_eval::controls::memory_retrieval::pre_retrieval_probe_results(probes),
    );
    assert!(!scores.is_empty(), "control produced no probe-type slices");
    for (slice, score) in &scores {
        assert_eq!(
            *score, 0.0,
            "pre-retrieval slice {slice} scored {score} with an empty store"
        );
    }
    Ok(())
}
