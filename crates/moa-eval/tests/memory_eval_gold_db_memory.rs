include!("memory_eval_support/common.rs");

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
