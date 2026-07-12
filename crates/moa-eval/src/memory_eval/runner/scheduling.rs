//! End-to-end eval phase scheduling and budget checkpoints.

use super::*;

pub(super) async fn run_memory_retrieval_eval_in_store(
    options: &MemoryRetrievalEvalOptions,
    corpus: LoadedMemoryEvalCorpus,
    store: &IsolatedEvalStore,
) -> Result<MemoryRetrievalEvalReport> {
    cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    let providers = options.providers_for_corpus(&corpus).await?;
    seed_eval_storage_partition_embedder_state(
        store.pool(),
        &corpus.ledger,
        providers.embedder.as_ref(),
    )
    .await?;
    let ingest_ctx = store.ingest_ctx(
        providers.embedder.clone(),
        providers.extractor.clone(),
        providers.entity_merge_verifier.clone(),
        providers.entity_blocking_enabled,
    );
    let mut gold_resolution =
        resolve_gold_nodes(ingest_ctx, &corpus.ledger, &corpus.sessions).await?;
    apply_eval_validity_windows(store.pool(), &mut gold_resolution).await?;
    stabilize_eval_access_times(store.pool(), &corpus.ledger).await?;
    let ranking_reference_time = deterministic_ranking_reference_time(&corpus.ledger);
    let consolidation_reference_time = deterministic_consolidation_reference_time(&corpus.ledger);
    let fact_ids_by_uid = fact_ids_by_uid(&gold_resolution);
    let consolidation = if options.consolidate() {
        let outcome = run_eval_consolidation(
            store.pool(),
            &corpus.ledger,
            &gold_resolution,
            &fact_ids_by_uid,
            providers.embedder.clone(),
            consolidation_reference_time,
            digest_config_for_eval(options.digests()),
        )
        .await?;
        Some(outcome)
    } else if options.digests() {
        Some(
            run_eval_digest_rebuild(store.pool(), &corpus.ledger, consolidation_reference_time)
                .await?,
        )
    } else {
        None
    };
    let mut equivalent_fact_ids_by_uid = if options.consolidate() {
        equivalent_fact_ids_by_uid(store.pool(), &corpus.ledger, &fact_ids_by_uid).await?
    } else {
        HashMap::new()
    };
    merge_equivalents(
        &mut equivalent_fact_ids_by_uid,
        content_equivalent_fact_ids(&corpus.ledger, &fact_ids_by_uid),
    );
    seed_eval_quality_scores(
        store.pool(),
        &corpus.ledger,
        &gold_resolution,
        options.invert_quality_priors(),
    )
    .await?;
    let extraction_precision =
        extraction_precision_counts(store.pool(), &corpus.ledger, &fact_ids_by_uid).await?;
    let entity_fragmentation = entity_fragmentation_counts(store.pool(), &corpus.ledger).await?;
    if let Err(error) = check_budget(&providers.ledger).await {
        let report = build_eval_report(ReportBuildInput {
            manifest: corpus.manifest,
            gold_resolution,
            probe_results: Vec::new(),
            bootstrap_config: options.bootstrap_config,
            extraction_precision,
            entity_fragmentation,
            reranker_enabled: options.reranker_enabled(),
            rewrite_summary: QueryRewriteSummary::empty(options.rewrite_policy()),
            graph_expansion_policy: options.graph_expansion_policy,
            aborted_over_budget: true,
            cost: Some(cost_snapshot(&providers.ledger).await),
            providers: Some(providers.provenance),
            consolidation: consolidation.clone(),
        });
        write_report(options.output_path(), &report).await?;
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
        return Err(error.into());
    }
    let gold_records_by_fact_id = gold_records_by_fact_id(&gold_resolution);
    let ledger_by_fact_id = ledger_by_fact_id(&corpus.ledger);
    let digest_context = if options.digests() {
        digest_context_by_user(store.pool(), &corpus.ledger).await?
    } else {
        HashMap::new()
    };
    let planner = QueryPlanner::new();
    let mut probe_results = Vec::with_capacity(corpus.probes.len());
    let mut rewrite_accounting = QueryRewriteAccounting::new(options.rewrite_policy());

    for (probe_index, probe) in corpus.probes.iter().enumerate() {
        let rewrite_decision = rewrite_accounting.record(probe);
        let retrieval_probe = probe_for_rewrite_policy(probe, rewrite_decision);
        let retrieval = retrieve_probe(
            store.pool(),
            &planner,
            providers.embedder.as_ref(),
            providers.reranker.clone(),
            &retrieval_probe,
            ProbeRetrieveOptions {
                use_reranker: options.reranker_enabled(),
                ranking_config: options.lane_ranking_config(),
                ranking_reference_time: Some(ranking_reference_time),
                deterministic_replay: providers.deterministic_replay,
                graph_expansion_policy: options.graph_expansion_policy,
            },
        )
        .await?;
        let candidates = candidates_from_retrieval_hits(
            &retrieval.pre_rerank_hits,
            &fact_ids_by_uid,
            &equivalent_fact_ids_by_uid,
            &probe.query,
        );
        let graph_comparison =
            retrieval
                .graph_off_retrieval_latency_ms
                .map(|graph_off_retrieval_latency_ms| {
                    let graph_off_candidates = candidates_from_retrieval_hits(
                        &retrieval.graph_off_hits,
                        &fact_ids_by_uid,
                        &equivalent_fact_ids_by_uid,
                        &probe.query,
                    );
                    probe_graph_comparison(
                        &probe.expected_fact_ids,
                        candidates.as_slice(),
                        graph_off_candidates,
                        &retrieval.graph_diagnostics,
                        graph_off_retrieval_latency_ms,
                    )
                });
        let post_rerank_candidates = candidates_from_retrieval_hits(
            &retrieval.post_rerank_hits,
            &fact_ids_by_uid,
            &equivalent_fact_ids_by_uid,
            &probe.query,
        );
        let preference_context_hit = preference_context_hit(
            probe,
            post_rerank_candidates.as_slice(),
            &digest_context,
            &ledger_by_fact_id,
        );
        probe_results.push(probe_result_for(ProbeResultInput {
            probe,
            candidates,
            post_rerank_candidates: Some(post_rerank_candidates),
            retrieval_latency_ms: retrieval.retrieval_latency_ms,
            gold_records_by_fact_id: &gold_records_by_fact_id,
            preference_context_hit,
            graph_diagnostics: Some(retrieval.graph_diagnostics),
            graph_comparison,
        }));
        if options.lane == EvalLane::Live
            && (probe_index + 1) % 10 == 0
            && let Err(error) = check_budget(&providers.ledger).await
        {
            let report = build_eval_report(ReportBuildInput {
                manifest: corpus.manifest,
                gold_resolution,
                probe_results,
                bootstrap_config: options.bootstrap_config,
                extraction_precision,
                entity_fragmentation,
                reranker_enabled: options.reranker_enabled(),
                rewrite_summary: rewrite_accounting.summary(),
                graph_expansion_policy: options.graph_expansion_policy,
                aborted_over_budget: true,
                cost: Some(cost_snapshot(&providers.ledger).await),
                providers: Some(providers.provenance),
                consolidation: consolidation.clone(),
            });
            write_report(options.output_path(), &report).await?;
            cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
            return Err(error.into());
        }
    }

    if let Err(error) = check_budget(&providers.ledger).await {
        let report = build_eval_report(ReportBuildInput {
            manifest: corpus.manifest,
            gold_resolution,
            probe_results,
            bootstrap_config: options.bootstrap_config,
            extraction_precision,
            entity_fragmentation,
            reranker_enabled: options.reranker_enabled(),
            rewrite_summary: rewrite_accounting.summary(),
            graph_expansion_policy: options.graph_expansion_policy,
            aborted_over_budget: true,
            cost: Some(cost_snapshot(&providers.ledger).await),
            providers: Some(providers.provenance),
            consolidation: consolidation.clone(),
        });
        write_report(options.output_path(), &report).await?;
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
        return Err(error.into());
    }

    let report = build_eval_report(ReportBuildInput {
        manifest: corpus.manifest,
        gold_resolution,
        probe_results,
        bootstrap_config: options.bootstrap_config,
        extraction_precision,
        entity_fragmentation,
        reranker_enabled: options.reranker_enabled(),
        rewrite_summary: rewrite_accounting.summary(),
        graph_expansion_policy: options.graph_expansion_policy,
        aborted_over_budget: false,
        cost: Some(cost_snapshot(&providers.ledger).await),
        providers: Some(providers.provenance),
        consolidation,
    });
    write_report(options.output_path(), &report).await?;
    if env::var("MOA_EVAL_KEEP_STORE").is_err() {
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    }
    Ok(report)
}
