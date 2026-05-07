//! Memory search, retrieval, show, and ingest command handlers.

use super::*;

pub(crate) async fn memory_search_report(
    config: &MoaConfig,
    query: &str,
    limit: usize,
) -> Result<String> {
    let graph = load_graph_store(config).await?;
    let seed_limit = i64::try_from(limit.max(1)).context("memory search limit is too large")?;
    let seeds = graph
        .lookup_seeds(query, seed_limit)
        .await?
        .into_iter()
        .map(|row| row.uid)
        .collect::<Vec<_>>();
    let retriever = load_hybrid_retriever(config).await?;
    let hits = retriever
        .retrieve(RetrievalRequest {
            seeds,
            query_text: query.to_string(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            },
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: limit,
            use_reranker: true,
            strategy: None,
        })
        .await?;

    let mut report = String::new();
    if hits.is_empty() {
        report.push_str("no hits\n");
        return Ok(report);
    }

    report.push_str("uid\tlabel\tname\tscore\tsnippet\n");
    for hit in hits {
        report.push_str(&format!(
            "{}\t{}\t{}\t{:.3}\t{}\n",
            hit.uid,
            hit.node.label.as_str(),
            sanitize_table_cell(&hit.node.name),
            hit.score,
            sanitize_table_cell(&node_snippet(&hit.node))
        ));
    }
    Ok(report)
}

pub(crate) async fn retrieve_report(config: &MoaConfig, args: &RetrieveArgs) -> Result<String> {
    if !args.debug {
        return memory_search_report(config, &args.query, args.limit).await;
    }

    memory_retrieve_debug_report(config, &args.query, args.limit, args.no_flush_wait).await
}

pub(crate) async fn memory_retrieve_debug_report(
    config: &MoaConfig,
    query: &str,
    limit: usize,
    no_flush_wait: bool,
) -> Result<String> {
    let graph = load_graph_store(config).await?;
    let seed_limit = i64::try_from(limit.max(1)).context("retrieve limit is too large")?;
    let seeds = graph
        .lookup_seeds(query, seed_limit)
        .await?
        .into_iter()
        .map(|row| row.uid)
        .collect::<Vec<_>>();
    let retriever = load_hybrid_retriever(config).await?;
    let hits = retriever
        .retrieve(RetrievalRequest {
            seeds,
            query_text: query.to_string(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Workspace {
                workspace_id: current_workspace_id(),
            },
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: limit,
            use_reranker: true,
            strategy: None,
        })
        .await?;
    let lineage_turn = if config.observability.lineage.enabled && !no_flush_wait {
        Some(record_debug_retrieval_lineage(config, query, &hits).await?)
    } else {
        None
    };

    let mut report = String::new();
    report.push_str("# retrieval debug\n");
    report.push_str(&format!("query: {query}\n"));
    report.push_str(&format!(
        "lineage_enabled: {}\n",
        config.observability.lineage.enabled
    ));
    report.push_str(&format!("no_flush_wait: {no_flush_wait}\n\n"));
    if let Some(turn_id) = lineage_turn {
        report.push_str(&format!("lineage_turn: {}\n\n", turn_id.0));
    }
    if hits.is_empty() {
        report.push_str("no hits\n");
        return Ok(report);
    }

    report.push_str("rank\tuid\tlabel\tname\tscore\tlegs\tsnippet\n");
    for (rank, hit) in hits.iter().enumerate() {
        report.push_str(&format!(
            "{}\t{}\t{}\t{}\t{:.3}\t{}\t{}\n",
            rank + 1,
            hit.uid,
            hit.node.label.as_str(),
            sanitize_table_cell(&hit.node.name),
            hit.score,
            leg_trace(hit.legs),
            sanitize_table_cell(&node_snippet(&hit.node))
        ));
    }
    Ok(report)
}

pub(crate) async fn record_debug_retrieval_lineage(
    config: &MoaConfig,
    query: &str,
    hits: &[moa_brain::retrieval::RetrievalHit],
) -> Result<TurnId> {
    let store = load_session_store(config).await?;
    let (sink, writer) = MpscSink::spawn(
        MpscSinkConfig::from(&config.observability.lineage),
        store.pool().clone(),
    )
    .await
    .context("starting lineage writer for retrieve --debug")?;
    let turn_id = TurnId::new_v7();
    let record = RetrievalLineage {
        turn_id,
        session_id: SessionId::new(),
        workspace_id: current_workspace_id(),
        user_id: current_user_id(),
        scope: MemoryScope::Workspace {
            workspace_id: current_workspace_id(),
        },
        ts: Utc::now(),
        query_original: query.to_string(),
        query_expansions: Vec::new(),
        vector_hits: hits
            .iter()
            .map(|hit| VecHit {
                chunk_id: hit.uid,
                score: hit.score as f32,
                source: "hybrid".to_string(),
                embedder: "debug".to_string(),
                embed_dim: memory_vector::VECTOR_DIMENSION as u16,
            })
            .collect(),
        graph_paths: Vec::new(),
        fusion_scores: hits
            .iter()
            .map(|hit| FusedHit {
                chunk_id: hit.uid,
                fused_score: hit.score as f32,
                vector_contribution: if hit.legs.vector { 1.0 } else { 0.0 },
                graph_contribution: if hit.legs.graph { 1.0 } else { 0.0 },
                lexical_contribution: if hit.legs.lexical { 1.0 } else { 0.0 },
                fusion_method: "rrf".to_string(),
            })
            .collect(),
        rerank_scores: hits
            .iter()
            .enumerate()
            .map(|(idx, hit)| RerankHit {
                chunk_id: hit.uid,
                original_index: idx.min(u16::MAX as usize) as u16,
                relevance_score: hit.score as f32,
                rerank_model: "debug".to_string(),
            })
            .collect(),
        top_k: hits.iter().map(|hit| hit.uid).collect(),
        timings: StageTimings::default(),
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    };
    let json = serde_json::to_value(LineageEvent::Retrieval(record))
        .context("serializing retrieve --debug lineage")?;
    sink.record(json);
    writer
        .shutdown()
        .await
        .context("flushing retrieve --debug lineage")?;
    Ok(turn_id)
}

pub(crate) async fn memory_show_report(config: &MoaConfig, uid_str: &str) -> Result<String> {
    let uid = Uuid::parse_str(uid_str).with_context(|| format!("invalid node uid `{uid_str}`"))?;
    let store = load_graph_store(config).await?;
    let node = store
        .get_node(uid)
        .await?
        .with_context(|| format!("node {uid} not found"))?;
    let neighbors = store.neighbors(uid, 1, None).await.unwrap_or_default();
    let properties = node
        .properties_summary
        .unwrap_or_else(|| serde_json::json!({}));

    let mut report = format!(
        "uid: {}\nlabel: {}\nname: {}\nscope: {}\nvalid_from: {}\nvalid_to: {}\n\nproperties:\n{}\n",
        node.uid,
        node.label.as_str(),
        node.name,
        node.scope,
        node.valid_from.to_rfc3339(),
        node.valid_to
            .map(|timestamp| timestamp.to_rfc3339())
            .unwrap_or_else(|| "<open>".to_string()),
        serde_json::to_string_pretty(&properties)?,
    );
    if !neighbors.is_empty() {
        report.push_str("\nneighbors:\n");
        for neighbor in neighbors {
            report.push_str(&format!(
                "- {} {} {}\n",
                neighbor.uid,
                neighbor.label.as_str(),
                neighbor.name
            ));
        }
    }
    Ok(report)
}

pub(crate) async fn memory_ingest_report(
    config: &MoaConfig,
    files: &[PathBuf],
    name: Option<&str>,
    workspace: Option<&str>,
) -> Result<String> {
    if files.is_empty() {
        bail!("at least one file path is required");
    }
    if files.len() > 1 && name.is_some() {
        bail!("--name can only be used when ingesting a single file");
    }

    let workspace_id = workspace
        .map(resolve_workspace_arg)
        .unwrap_or_else(current_workspace_id);
    let vo = load_ingestion_vo(config).await?;

    let mut sections = Vec::with_capacity(files.len());
    for file in files {
        let content = fs::read_to_string(file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let source_name = match name {
            Some(value) => value.to_string(),
            None => derive_ingest_source_name(file),
        };
        let turn = synthesize_cli_ingest_turn(&workspace_id, &source_name, &content);
        let report = vo.ingest_turn(turn).await?;
        sections.push(format_cli_ingest_section(file, &source_name, &report));
    }

    let mut output = String::new();
    if files.len() > 1 {
        output.push_str(&format!(
            "Ingested {} documents into workspace memory.\n\n",
            files.len()
        ));
    }
    output.push_str(&sections.join("\n\n"));
    output.push('\n');
    Ok(output)
}

pub(crate) fn derive_ingest_source_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unnamed-source");
    stem.split(['-', '_', ' '])
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn synthesize_cli_ingest_turn(
    workspace_id: &WorkspaceId,
    source_name: &str,
    content: &str,
) -> SessionTurn {
    SessionTurn {
        workspace_id: workspace_id.clone(),
        user_id: current_user_id(),
        session_id: SessionId::new(),
        turn_seq: 1,
        transcript: format!("source: {source_name}\n\n{content}"),
        dominant_pii_class: "none".to_string(),
        finalized_at: Utc::now(),
    }
}

pub(crate) fn format_cli_ingest_section(
    path: &Path,
    source_name: &str,
    report: &IngestApplyReport,
) -> String {
    let mut lines = vec![
        format!("Ingested \"{}\" ({})", source_name, path.display()),
        format!(
            "nodes: inserted={} superseded={} skipped={} failed={}",
            report.inserted, report.superseded, report.skipped, report.failed
        ),
        "edges: 0".to_string(),
        "contradictions: 0".to_string(),
    ];

    if report.failed > 0 {
        lines.push("dead_lettered: see moa.ingest_dlq".to_string());
    }

    lines.join("\n")
}

pub(crate) fn node_snippet(node: &memory_graph::NodeIndexRow) -> String {
    let Some(properties) = &node.properties_summary else {
        return String::new();
    };
    if let Some(value) = properties
        .get("summary")
        .and_then(serde_json::Value::as_str)
    {
        return value.to_string();
    }
    if let Some(value) = properties.get("object").and_then(serde_json::Value::as_str) {
        return value.to_string();
    }
    properties.to_string()
}

pub(crate) fn sanitize_table_cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\t', " ")
}

pub(crate) fn env_presence(key: &str) -> &'static str {
    if env::var(key).is_ok() {
        "present"
    } else {
        "missing"
    }
}
