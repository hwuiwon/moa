//! Retrieval probe execution and graph-impact diagnostics.

use super::*;

pub(super) async fn retrieve_probe(
    pool: &PgPool,
    planner: &QueryPlanner,
    embedder: &dyn EmbeddingProvider,
    reranker: Arc<dyn Reranker>,
    probe: &Probe,
    options: ProbeRetrieveOptions,
) -> Result<ProbeRetrieval> {
    let ProbeRetrieveOptions {
        use_reranker,
        ranking_config,
        ranking_reference_time,
        deterministic_replay,
        graph_expansion_policy,
    } = options;
    let started = Instant::now();
    // The lane's ranking config is the source of the request window policy;
    // the retriever no longer imposes these knobs globally.
    let window_policy = EvidenceWindowPolicy {
        rerank_window: ranking_config.rerank_window,
        abstain_below_window_evidence: ranking_config.abstain_below_window_evidence,
    };
    let scope = MemoryScope::Contact {
        tenant_id: tenant_id_from_storage_partition_id(&probe.storage_partition_id),
        contact_id: contact_id_from_user_id(&probe.user_id),
    };
    let scope_context = scope.to_rls_context();
    let mut vector_store = PgvectorStore::new_for_app_role(pool.clone(), scope_context.clone());
    if deterministic_replay {
        vector_store = vector_store.with_exact_search(true);
    }
    let vector = Arc::new(vector_store);
    let graph_vector: Arc<dyn VectorStore> = vector.clone();
    let graph_store = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_context)
        .with_vector_store(graph_vector);
    let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
    let hybrid = HybridRetriever::new(pool.clone(), graph.clone(), vector)
        .with_ranking_config(ranking_config)
        .with_reranker(reranker)
        .with_assume_app_role(true);
    let planning = PlanningCtx::new(scope, graph);
    let planned = planner
        .plan(&probe.query, &planning)
        .await
        .map_err(|error| memory_retrieval_error(probe, error))?;
    let query_embedding = embed_probe_query(embedder, probe).await?;

    let pre_rerank_output = retrieve_probe_output(
        &hybrid,
        &planned,
        probe,
        query_embedding.clone(),
        ProbeHitOptions {
            k_final: RETRIEVAL_EVAL_CANDIDATE_K,
            use_reranker: false,
            ranking_reference_time,
            graph_expansion_policy,
            force_graph_off: false,
            window_policy,
        },
    )
    .await?;
    let post_rerank_hits = if use_reranker {
        retrieve_probe_output(
            &hybrid,
            &planned,
            probe,
            query_embedding.clone(),
            ProbeHitOptions {
                k_final: RETRIEVAL_EVAL_FINAL_K,
                use_reranker: true,
                ranking_reference_time,
                graph_expansion_policy,
                force_graph_off: false,
                window_policy,
            },
        )
        .await?
        .hits
    } else {
        pre_rerank_output
            .hits
            .iter()
            .take(RETRIEVAL_EVAL_FINAL_K)
            .cloned()
            .collect()
    };
    let primary_retrieval_latency_ms = if deterministic_replay {
        0
    } else {
        duration_ms_u64(started.elapsed())
    };
    let (graph_off_hits, graph_off_retrieval_latency_ms) =
        if should_compare_graph(pre_rerank_output.diagnostics.policy) {
            let graph_off_started = Instant::now();
            let graph_off_output = retrieve_probe_output(
                &hybrid,
                &planned,
                probe,
                query_embedding,
                ProbeHitOptions {
                    k_final: RETRIEVAL_EVAL_CANDIDATE_K,
                    use_reranker: false,
                    ranking_reference_time,
                    graph_expansion_policy,
                    force_graph_off: true,
                    window_policy,
                },
            )
            .await?;
            (
                graph_off_output.hits,
                Some(if deterministic_replay {
                    0
                } else {
                    duration_ms_u64(graph_off_started.elapsed())
                }),
            )
        } else {
            (Vec::new(), None)
        };

    Ok(ProbeRetrieval {
        pre_rerank_hits: pre_rerank_output.hits,
        post_rerank_hits,
        graph_diagnostics: pre_rerank_output.diagnostics,
        graph_off_hits,
        graph_off_retrieval_latency_ms,
        retrieval_latency_ms: primary_retrieval_latency_ms,
    })
}

pub(super) struct ProbeRetrieveOptions {
    pub(super) use_reranker: bool,
    pub(super) ranking_config: RankingConfig,
    pub(super) ranking_reference_time: Option<DateTime<Utc>>,
    pub(super) deterministic_replay: bool,
    pub(super) graph_expansion_policy: GraphExpansionEvalPolicy,
}

pub(super) fn should_skip_graph_expansion_for_exact_direct_probe(
    planned: &PlannedQuery,
    req: &RetrievalRequest,
) -> bool {
    req.as_of.is_none() && should_skip_graph_expansion_for_direct_lookup(planned, &req.query_text)
}

pub(super) async fn embed_probe_query(
    embedder: &dyn EmbeddingProvider,
    probe: &Probe,
) -> Result<Vec<f32>> {
    let query_input = vec![probe.query.clone()];
    let mut embeddings = embedder.embed(&query_input).await.map_err(|error| {
        EvalError::InvalidConfig(format!(
            "memory query embedding failed for probe {}: {error}",
            probe.probe_id
        ))
    })?;
    embeddings.pop().ok_or_else(|| {
        EvalError::InvalidConfig(format!(
            "memory query embedding returned no vector for probe {}",
            probe.probe_id
        ))
    })
}

pub(super) async fn retrieve_probe_output(
    hybrid: &HybridRetriever,
    planned: &PlannedQuery,
    probe: &Probe,
    query_embedding: Vec<f32>,
    options: ProbeHitOptions,
) -> Result<RetrievalOutput> {
    let request = probe_retrieval_request(planned, probe, query_embedding, options);
    hybrid
        .retrieve_with_diagnostics(request)
        .await
        .map_err(|error| memory_retrieval_error(probe, error))
}

pub(super) fn probe_retrieval_request(
    planned: &PlannedQuery,
    probe: &Probe,
    query_embedding: Vec<f32>,
    options: ProbeHitOptions,
) -> RetrievalRequest {
    let mut request = planned.clone().into_retrieval_request(
        &probe.query,
        query_embedding,
        PiiClass::Restricted,
        options.k_final,
        options.use_reranker,
    );
    request.window_policy = options.window_policy;
    request.ranking_reference_time = options.ranking_reference_time;
    request.disable_leg_timeouts = true;
    request.disable_graph_expansion = options.force_graph_off
        || should_skip_graph_expansion_for_direct_lookup(planned, &request.query_text)
        || (options.graph_expansion_policy == GraphExpansionEvalPolicy::SkipExactDirect
            && should_skip_graph_expansion_for_exact_direct_probe(planned, &request));
    request
}

pub(super) fn memory_retrieval_error(
    probe: &Probe,
    error: impl std::fmt::Display,
) -> moa_eval_core::EvalError {
    EvalError::InvalidConfig(format!(
        "memory retrieval failed for probe {}: {error}",
        probe.probe_id
    ))
}

pub(super) fn should_compare_graph(policy: GraphRetrievalPolicy) -> bool {
    !matches!(
        policy,
        GraphRetrievalPolicy::Off | GraphRetrievalPolicy::ContextOnly
    )
}

pub(super) fn probe_graph_comparison(
    expected_fact_ids: &[String],
    graph_candidates: &[RetrievedCandidate],
    graph_off_candidates: Vec<RetrievedCandidate>,
    graph_diagnostics: &GraphRetrievalDiagnostics,
    graph_off_retrieval_latency_ms: u64,
) -> ProbeGraphComparison {
    let relevant_rank_with_graph = first_relevant_rank(graph_candidates, expected_fact_ids);
    let relevant_rank_without_graph = first_relevant_rank(&graph_off_candidates, expected_fact_ids);
    let impact = classify_graph_impact(relevant_rank_with_graph, relevant_rank_without_graph);
    let top_harmful_graph_paths = if impact == GraphImpact::Hurt {
        top_harmful_graph_paths(graph_diagnostics, graph_candidates, expected_fact_ids)
    } else {
        Vec::new()
    };
    ProbeGraphComparison {
        impact,
        relevant_rank_with_graph,
        relevant_rank_without_graph,
        rank_delta_with_minus_without: rank_delta(
            relevant_rank_with_graph,
            relevant_rank_without_graph,
        ),
        graph_off_candidates,
        top_harmful_graph_paths,
        graph_off_retrieval_latency_ms,
    }
}

pub(super) fn classify_graph_impact(
    rank_with_graph: Option<usize>,
    rank_without_graph: Option<usize>,
) -> GraphImpact {
    match rank_order_value(rank_with_graph).cmp(&rank_order_value(rank_without_graph)) {
        std::cmp::Ordering::Less => GraphImpact::Rescue,
        std::cmp::Ordering::Equal => GraphImpact::Neutral,
        std::cmp::Ordering::Greater => GraphImpact::Hurt,
    }
}

pub(super) fn rank_order_value(rank: Option<usize>) -> usize {
    rank.unwrap_or(usize::MAX)
}

pub(super) fn rank_delta(
    rank_with_graph: Option<usize>,
    rank_without_graph: Option<usize>,
) -> Option<i64> {
    match (rank_with_graph, rank_without_graph) {
        (Some(with_graph), Some(without_graph)) => Some(with_graph as i64 - without_graph as i64),
        _ => None,
    }
}

pub(super) fn first_relevant_rank(
    candidates: &[RetrievedCandidate],
    expected_fact_ids: &[String],
) -> Option<usize> {
    let expected = expected_fact_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if expected.is_empty() {
        return None;
    }
    candidates
        .iter()
        .filter(|candidate| candidate_matches_expected(candidate, &expected))
        .map(|candidate| candidate.rank)
        .min()
}

pub(super) fn candidate_matches_expected(
    candidate: &RetrievedCandidate,
    expected: &std::collections::BTreeSet<&str>,
) -> bool {
    candidate
        .fact_id
        .as_deref()
        .is_some_and(|fact_id| expected.contains(fact_id))
        || candidate
            .equivalent_fact_ids
            .iter()
            .any(|fact_id| expected.contains(fact_id.as_str()))
}

pub(super) fn top_harmful_graph_paths(
    diagnostics: &GraphRetrievalDiagnostics,
    graph_candidates: &[RetrievedCandidate],
    expected_fact_ids: &[String],
) -> Vec<ProbeGraphPathDiagnostic> {
    let expected = expected_fact_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let contexts = graph_candidate_contexts(graph_candidates);
    let harmful_uids = graph_candidates
        .iter()
        .filter(|candidate| candidate.legs.graph)
        .filter(|candidate| !candidate_matches_expected(candidate, &expected))
        .map(|candidate| candidate.uid)
        .collect::<std::collections::HashSet<_>>();
    let graph_uids = graph_candidates
        .iter()
        .filter(|candidate| candidate.legs.graph)
        .map(|candidate| candidate.uid)
        .collect::<std::collections::HashSet<_>>();
    let mut paths =
        graph_path_diagnostics_for_candidates(&diagnostics.path_traces, &contexts, &harmful_uids);
    if paths.is_empty() {
        paths =
            graph_path_diagnostics_for_candidates(&diagnostics.path_traces, &contexts, &graph_uids);
    }
    if paths.is_empty() {
        paths = diagnostics
            .path_traces
            .iter()
            .map(ProbeGraphPathDiagnostic::from)
            .collect();
    }
    paths.sort_by(|left, right| {
        left.candidate_rank_with_graph
            .unwrap_or(usize::MAX)
            .cmp(&right.candidate_rank_with_graph.unwrap_or(usize::MAX))
            .then_with(|| left.hop.cmp(&right.hop))
            .then_with(|| left.seed_uid.cmp(&right.seed_uid))
            .then_with(|| left.candidate_uid.cmp(&right.candidate_uid))
            .then_with(|| left.edge_labels.cmp(&right.edge_labels))
    });
    paths.truncate(5);
    paths
}

pub(super) fn graph_candidate_contexts(
    candidates: &[RetrievedCandidate],
) -> HashMap<Uuid, ProbeGraphCandidateContext> {
    let mut contexts = HashMap::new();
    for candidate in candidates.iter().filter(|candidate| candidate.legs.graph) {
        contexts
            .entry(candidate.uid)
            .or_insert(ProbeGraphCandidateContext {
                rank: candidate.rank,
                fact_id: candidate.fact_id.clone(),
            });
    }
    contexts
}

pub(super) fn graph_path_diagnostics_for_candidates(
    traces: &[GraphPathTrace],
    contexts: &HashMap<Uuid, ProbeGraphCandidateContext>,
    candidate_uids: &std::collections::HashSet<Uuid>,
) -> Vec<ProbeGraphPathDiagnostic> {
    traces
        .iter()
        .filter(|trace| candidate_uids.contains(&trace.candidate_uid))
        .map(|trace| {
            let context = contexts.get(&trace.candidate_uid);
            ProbeGraphPathDiagnostic {
                seed_uid: trace.seed_uid,
                seed_source: trace.seed_source,
                candidate_uid: trace.candidate_uid,
                candidate_rank_with_graph: context.map(|context| context.rank),
                candidate_fact_id: context.and_then(|context| context.fact_id.clone()),
                hop: trace.hop,
                edge_labels: trace.edge_labels.clone(),
            }
        })
        .collect()
}

pub(super) struct ProbeGraphCandidateContext {
    rank: usize,
    fact_id: Option<String>,
}

pub(super) fn deterministic_ranking_reference_time(ledger: &[LedgerFact]) -> DateTime<Utc> {
    ledger
        .iter()
        .map(|fact| fact.valid_from)
        .max()
        .unwrap_or_else(Utc::now)
        + chrono::Duration::days(7)
}

pub(super) fn deterministic_consolidation_reference_time(ledger: &[LedgerFact]) -> DateTime<Utc> {
    ledger
        .iter()
        .map(|fact| fact.valid_from)
        .min()
        .unwrap_or_else(Utc::now)
        + chrono::Duration::days(7)
}

pub(super) struct ProbeRetrieval {
    pub(super) pre_rerank_hits: Vec<RetrievalHit>,
    pub(super) post_rerank_hits: Vec<RetrievalHit>,
    pub(super) graph_diagnostics: GraphRetrievalDiagnostics,
    pub(super) graph_off_hits: Vec<RetrievalHit>,
    pub(super) graph_off_retrieval_latency_ms: Option<u64>,
    pub(super) retrieval_latency_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ProbeHitOptions {
    k_final: usize,
    use_reranker: bool,
    ranking_reference_time: Option<DateTime<Utc>>,
    graph_expansion_policy: GraphExpansionEvalPolicy,
    force_graph_off: bool,
    window_policy: EvidenceWindowPolicy,
}

pub(super) fn duration_ms_u64(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(super) struct ProbeResultInput<'a> {
    pub(super) probe: &'a Probe,
    pub(super) candidates: Vec<RetrievedCandidate>,
    pub(super) post_rerank_candidates: Option<Vec<RetrievedCandidate>>,
    pub(super) retrieval_latency_ms: u64,
    pub(super) gold_records_by_fact_id: &'a HashMap<String, crate::memory_eval::GoldNodeRecord>,
    pub(super) preference_context_hit: Option<bool>,
    pub(super) graph_diagnostics: Option<GraphRetrievalDiagnostics>,
    pub(super) graph_comparison: Option<ProbeGraphComparison>,
}

pub(super) fn probe_result_for(input: ProbeResultInput<'_>) -> ProbeResult {
    let ProbeResultInput {
        probe,
        candidates,
        post_rerank_candidates,
        retrieval_latency_ms,
        gold_records_by_fact_id,
        preference_context_hit,
        graph_diagnostics,
        graph_comparison,
    } = input;
    let final_candidates = post_rerank_candidates.as_deref().unwrap_or(&candidates);
    let all_expected_found_at_4_value = all_expected_found_at_k(
        final_candidates,
        &probe.expected_fact_ids,
        RETRIEVAL_EVAL_FINAL_K,
    );
    let forbidden_fact_absent_at_4_value = !any_blocked_found_at_k(
        final_candidates,
        &probe.blocked_fact_ids,
        RETRIEVAL_EVAL_FINAL_K,
    );
    let stored_pii_redacted = stored_pii_redacted_for_probe(probe, gold_records_by_fact_id);
    let all_expected_found_at_4 =
        (!probe.expected_fact_ids.is_empty()).then_some(all_expected_found_at_4_value);
    let forbidden_fact_absent_at_4 = matches!(
        probe.probe_type,
        ProbeType::Abstention | ProbeType::CrossUserIsolation
    )
    .then_some(forbidden_fact_absent_at_4_value);
    let retrieval_temporal_as_of_correct = (probe.probe_type == ProbeType::TemporalAsOf)
        .then_some(all_expected_found_at_4_value && forbidden_fact_absent_at_4_value);
    let (temporal_filter_parsed, temporal_filter_matches_as_of) = temporal_parse_diagnostics(probe);

    ProbeResult {
        probe_id: probe.probe_id.clone(),
        user_id: probe.user_id.as_str().to_string(),
        probe_type: probe.probe_type,
        expected_fact_ids: probe.expected_fact_ids.clone(),
        expected_fact_grades: probe.expected_fact_grades.clone(),
        blocked_fact_ids: probe.blocked_fact_ids.clone(),
        candidates,
        post_rerank_candidates,
        rendered_candidate_count: None,
        retrieval_latency_ms,
        all_expected_found_at_4,
        forbidden_fact_absent_at_4,
        stored_pii_redacted,
        retrieval_temporal_as_of_correct,
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
        preference_context_hit,
        graph_diagnostics,
        graph_comparison,
    }
}

pub(super) fn temporal_parse_diagnostics(probe: &Probe) -> (Option<bool>, Option<bool>) {
    if probe.probe_type != ProbeType::TemporalAsOf {
        return (None, None);
    }

    let parsed = parse_temporal(&probe.query);
    (
        Some(parsed.is_some()),
        parsed.map(|instant| Some(instant) == probe.as_of),
    )
}

pub(super) fn stored_pii_redacted_for_probe(
    probe: &Probe,
    gold_records_by_fact_id: &HashMap<String, crate::memory_eval::GoldNodeRecord>,
) -> Option<bool> {
    if probe.probe_type != ProbeType::PiiRedaction {
        return None;
    }

    let mut resolved_pii = false;
    for fact_id in &probe.expected_fact_ids {
        let Some(record) = gold_records_by_fact_id.get(fact_id) else {
            continue;
        };
        match record.pii_status {
            GoldPiiStatus::Unredacted | GoldPiiStatus::Mixed => return Some(false),
            GoldPiiStatus::Redacted => resolved_pii = true,
            GoldPiiStatus::NotExpected | GoldPiiStatus::NotResolved => {}
        }
    }
    resolved_pii.then_some(true)
}

pub(super) fn all_expected_found_at_k(
    candidates: &[RetrievedCandidate],
    expected: &[String],
    k: usize,
) -> bool {
    if expected.is_empty() {
        return false;
    }
    expected.iter().all(|expected_fact_id| {
        candidates.iter().any(|candidate| {
            candidate.rank > 0
                && candidate.rank <= k
                && candidate_fact_ids(candidate).any(|fact_id| fact_id == expected_fact_id)
        })
    })
}

pub(super) fn any_blocked_found_at_k(
    candidates: &[RetrievedCandidate],
    blocked: &[String],
    k: usize,
) -> bool {
    if blocked.is_empty() {
        return false;
    }
    candidates.iter().any(|candidate| {
        candidate.rank > 0
            && candidate.rank <= k
            && candidate_fact_ids(candidate)
                .any(|fact_id| blocked.iter().any(|blocked| blocked == fact_id))
    })
}

pub(super) fn candidate_fact_ids(candidate: &RetrievedCandidate) -> impl Iterator<Item = &str> {
    candidate
        .fact_id
        .as_deref()
        .into_iter()
        .chain(candidate.equivalent_fact_ids.iter().map(String::as_str))
}

pub(super) fn fact_ids_by_uid(gold_resolution: &GoldResolutionReport) -> HashMap<Uuid, String> {
    let mut fact_ids = HashMap::new();
    for record in &gold_resolution.records {
        for uid in &record.node_uids {
            fact_ids
                .entry(*uid)
                .or_insert_with(|| record.fact_id.clone());
        }
    }
    fact_ids
}

pub(super) async fn equivalent_fact_ids_by_uid(
    pool: &PgPool,
    ledger: &[LedgerFact],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<HashMap<Uuid, Vec<String>>> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT target_uid, (payload->>'replacement_uid')::uuid AS replacement_uid
        FROM moa.graph_changelog
        WHERE storage_partition_id = ANY($1)
          AND op = 'supersede'
          AND target_kind = 'node'
          AND target_label = 'Fact'
          AND payload ? 'replacement_uid'
        ORDER BY change_id ASC
        "#,
    )
    .bind(&storage_partition_ids)
    .fetch_all(pool)
    .await
    .map_err(crate::eval_sqlx_error)?;
    let mut replacement_by_old = HashMap::<Uuid, Uuid>::new();
    for row in rows {
        replacement_by_old.insert(
            row.try_get("target_uid").map_err(crate::eval_sqlx_error)?,
            row.try_get("replacement_uid")
                .map_err(crate::eval_sqlx_error)?,
        );
    }

    let mut aliases = HashMap::<Uuid, Vec<String>>::new();
    for (uid, fact_id) in fact_ids_by_uid {
        let representative = supersession_representative(*uid, &replacement_by_old);
        if representative != *uid {
            aliases
                .entry(representative)
                .or_default()
                .push(fact_id.clone());
        }
    }
    for fact_ids in aliases.values_mut() {
        fact_ids.sort();
        fact_ids.dedup();
    }
    Ok(aliases)
}

/// Maps each resolved fact uid to other ledger fact ids carrying identical
/// tenant-shared content in the same storage partition.
///
/// The corpus legitimately plants distinct gold facts with byte-identical
/// `(subject, predicate, object)` (two components depending on one library
/// owned by one team). Retrieval surfacing any duplicate-content row carries
/// the same evidence, so probes accept the group instead of one arbitrary
/// fact id. Contact-scope facts are excluded: they are never interchangeable
/// across contacts, and isolation probes must not gain accidental matches.
pub(super) fn content_equivalent_fact_ids(
    ledger: &[LedgerFact],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> HashMap<Uuid, Vec<String>> {
    let mut groups = HashMap::<(String, String, String, String), Vec<&str>>::new();
    for fact in ledger {
        if fact.scope != ScopeTier::Tenant {
            continue;
        }
        groups
            .entry((
                fact.storage_partition_id.to_string(),
                moa_memory_types::normalize_fact_component(&fact.subject),
                moa_memory_types::normalize_fact_component(&fact.predicate),
                moa_memory_types::normalize_fact_component(&fact.object),
            ))
            .or_default()
            .push(fact.fact_id.as_str());
    }
    let group_by_fact_id: HashMap<&str, &Vec<&str>> = groups
        .values()
        .filter(|group| group.len() > 1)
        .flat_map(|group| group.iter().map(move |fact_id| (*fact_id, group)))
        .collect();

    let mut aliases = HashMap::new();
    for (uid, fact_id) in fact_ids_by_uid {
        let Some(group) = group_by_fact_id.get(fact_id.as_str()) else {
            continue;
        };
        let mut others = group
            .iter()
            .filter(|other| **other != fact_id.as_str())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        others.sort();
        aliases.insert(*uid, others);
    }
    aliases
}

/// Merges additional equivalents into an existing uid-keyed alias map.
pub(super) fn merge_equivalents(
    target: &mut HashMap<Uuid, Vec<String>>,
    additional: HashMap<Uuid, Vec<String>>,
) {
    for (uid, fact_ids) in additional {
        let entry = target.entry(uid).or_default();
        entry.extend(fact_ids);
        entry.sort();
        entry.dedup();
    }
}

pub(super) fn supersession_representative(
    uid: Uuid,
    replacement_by_old: &HashMap<Uuid, Uuid>,
) -> Uuid {
    let mut current = uid;
    let mut seen = std::collections::BTreeSet::new();
    while seen.insert(current) {
        let Some(next) = replacement_by_old.get(&current).copied() else {
            return current;
        };
        current = next;
    }
    uid
}
