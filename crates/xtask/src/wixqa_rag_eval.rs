//! `xtask wixqa-rag-eval` command implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use moa_brain::planning::Strategy;
use moa_brain::retrieval::{
    GraphCandidateCounts, GraphPathTrace, GraphRetrievalDiagnostics, GraphRetrievalPolicy,
    GraphSeedDiagnostics, GraphSeedSource, HybridRetriever, LexicalBackend, RetrievalHit,
    RetrievalOutput, RetrievalRequest, SourceObjectFeatureContribution,
    SourceObjectFeatureContributions,
};
use moa_core::traits::EmbeddingProvider;
use moa_core::{config::MoaConfig, types::identifiers::TenantId, types::memory::RlsContext};
use moa_db::ScopedConn;
use moa_eval::kernel::cost::{
    COHERE_EMBED_V4_INPUT_USD_PER_MILLION_TOKENS, COHERE_RERANK_V4_FAST_USD_PER_SEARCH,
    PRICING_AS_OF,
};
use moa_knowledge::chunking::{ChunkingConfig, content_hash};
use moa_knowledge::domain::{
    ConnectionStatus, KnowledgeConnection, KnowledgeSyncRun, ProviderRecord, RecordPage,
    SyncRunStatus,
};
use moa_knowledge::graph_delta::stable_uid;
use moa_knowledge::ingestion::{
    KnowledgeIngestionPipeline, KnowledgeIngestionPipelineConfig, MemoryKnowledgeGraphWriter,
    PageIngestionReport,
};
use moa_knowledge::parser::native::NativeDocumentParser;
use moa_knowledge::repository::{KnowledgeRepository, PostgresKnowledgeRepository, SyncRunClaim};
use moa_memory_graph::{NodeLabel, PiiClass, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{
    VectorPartitionPromotion, VectorStore, VectorStoreFactory, VectorSyncReport,
};
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};
use pgvector::HalfVector;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

const DEFAULT_DATA_DIR: &str = ".moa/wixqa/raw";
const DEFAULT_OUTPUT: &str = ".moa/wixqa/reports/latest.json";
const WIXQA_PROVIDER: &str = "wixqa";
const WIXQA_CONNECTOR: &str = "wixqa_jsonl";
const WIXQA_DEFAULT_EMBEDDER: &str = "cohere:embed-v4.0";
const WIXQA_DEFAULT_EMBEDDING_DIMENSIONS: usize = 1024;
const WIXQA_GEMINI_EMBEDDER: &str = "gemini:gemini-embedding-2";
const GEMINI_EMBEDDING_2_INPUT_USD_PER_MILLION_TOKENS: f64 = 0.20;
const WIXQA_RERANKER: &str = "cohere:rerank-v4.0-fast";
const INGESTION_BATCH_SIZE: usize = 64;
const VECTOR_SYNC_DRAIN_LIMIT: i64 = 100_000;
const WEAK_REPEAT_FALLBACK_WINDOW: usize = 10;
const WEAK_REPEAT_FALLBACK_THRESHOLD: usize = 1;

/// Runs the WixQA RAG benchmark command.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    if options.help {
        print_help();
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for WixQA RAG eval")?;
    let summary = runtime.block_on(run_async(options))?;
    println!(
        "wrote WixQA RAG report: output={} dataset={} backend={} questions={} articles={} recall@{}={:.3} hit@{}={:.3} mrr={:.3} ndcg@{}={:.3} graph_hurt={} graph_rescue={} graph_neutral={} total_p95_ms={} est_usd={:.5}",
        summary.output.display(),
        summary.dataset,
        summary.backend,
        summary.question_count,
        summary.article_count,
        summary.metric_cutoff,
        summary.recall_at_k,
        summary.metric_cutoff,
        summary.hit_at_k,
        summary.mrr,
        summary.metric_cutoff,
        summary.ndcg_at_k,
        summary.graph_hurt_count,
        summary.graph_rescue_count,
        summary.graph_neutral_count,
        summary.total_p95_ms,
        summary.estimated_usd
    );
    Ok(())
}

async fn run_async(options: Options) -> Result<RunSummary> {
    let corpus = load_corpus(&options.data_dir, options.dataset)?;
    let questions = load_questions(&options.data_dir, options.dataset)?;
    let selected = select_workload(&corpus, &questions, &options)?;
    let config = benchmark_config(&options)?;
    let pool = PgPool::connect(&config.database.url)
        .await
        .context("connect to MOA Postgres")?;
    let ingestion_embedder =
        build_embedder_from_config(&config, EmbedderConstructionRole::Ingestion)
            .with_context(|| format!("build {} ingestion embedder", options.embedder_name))?;
    let retrieval_embedder =
        build_embedder_from_config(&config, EmbedderConstructionRole::Retrieval)
            .with_context(|| format!("build {} retrieval embedder", options.embedder_name))?;
    let tenant_id = TenantId::from(stable_uid(&selected.cache_key));
    let connection_uid = stable_uid(&format!("wixqa-connection:{}", selected.cache_key));
    let storage_partition_id = tenant_id.0.to_string();
    seed_storage_partition_state(
        &pool,
        tenant_id,
        &storage_partition_id,
        options.backend.as_str(),
        ingestion_embedder.as_ref(),
    )
    .await?;
    let (ingestion, vector_sync) = if options.skip_ingestion {
        let cached = validate_cached_workload(&pool, tenant_id, connection_uid, &selected).await?;
        if cached.article_count != selected.articles.len() as u64 {
            bail!(
                "--skip-ingestion requested but cache `{}` has {}/{} selected WixQA articles",
                selected.cache_key,
                cached.article_count,
                selected.articles.len()
            );
        }
        if cached.chunk_count == 0 {
            bail!(
                "--skip-ingestion requested but cache `{}` has no graph-linked WixQA chunks",
                selected.cache_key
            );
        }
        let vector_sync = if options.drain_vector_sync {
            drain_external_vector_sync(&pool, &config, options.vector_sync_drain_limit).await?
        } else {
            VectorSyncReport::default()
        };
        (IngestionReport::skipped(cached), vector_sync)
    } else {
        let ingestion = ingest_articles(
            &pool,
            tenant_id,
            connection_uid,
            &selected,
            &config,
            ingestion_embedder,
        )
        .await?;
        let vector_sync =
            drain_external_vector_sync(&pool, &config, options.vector_sync_drain_limit).await?;
        (ingestion, vector_sync)
    };
    // The Turbopuffer read side is a projection of the Postgres-canonical
    // embeddings. The incremental outbox drain above only carries rows written
    // by THIS run's ingestion, so a reused/already-ingested tenant would leave
    // the namespace empty. Reproject the whole partition so retrieval reads a
    // complete namespace regardless of ingestion skips.
    let turbopuffer_reprojected = if matches!(options.backend, Backend::Turbopuffer) {
        let copied = reproject_partition_to_turbopuffer(&pool, &config, tenant_id).await?;
        eprintln!(
            "reprojected {copied} partition embeddings into Turbopuffer (storage_partition_id={storage_partition_id}) before retrieval"
        );
        Some(copied as u64)
    } else {
        None
    };
    let query_run = retrieve_questions(RetrievalEvalInputs {
        pool: &pool,
        tenant_id,
        embedder: retrieval_embedder,
        selected: &selected,
        config: &config,
        top_k: options.top_k,
        use_reranker: options.reranker,
        disable_graph_expansion: options.disable_graph_expansion,
        graph_policy: options.graph_policy,
        weak_repeat_fallback_top_k: options.weak_repeat_fallback_top_k,
        weak_repeat_rerank: options.weak_repeat_rerank,
        capture_embedding_export_queries: options.embedding_export.is_some(),
        expected_embedding_dim: options.embedding_dim,
    })
    .await?;
    if let Some(embedding_export_path) = &options.embedding_export {
        let export = build_embedding_export(
            &pool,
            tenant_id,
            connection_uid,
            &selected,
            &options,
            query_run.embedding_export_queries,
        )
        .await?;
        write_pretty_json(embedding_export_path, &export).with_context(|| {
            format!("write embedding export {}", embedding_export_path.display())
        })?;
    }
    let mut report = build_report(
        &options,
        tenant_id,
        connection_uid,
        selected,
        ingestion,
        vector_sync,
        query_run.measurements,
    );
    report.turbopuffer_reprojected = turbopuffer_reprojected;
    write_pretty_json(&options.output, &report)
        .with_context(|| format!("write {}", options.output.display()))?;
    Ok(RunSummary {
        output: options.output,
        dataset: report.dataset,
        backend: report.backend,
        question_count: report.question_count,
        article_count: report.article_count,
        metric_cutoff: metric_cutoff_label(&report.fallback),
        recall_at_k: report.metrics.recall_at_k,
        hit_at_k: report.metrics.hit_at_k,
        mrr: report.metrics.mrr,
        ndcg_at_k: report.metrics.ndcg_at_k,
        graph_hurt_count: report.graph_diagnostics.graph_hurt_count,
        graph_rescue_count: report.graph_diagnostics.graph_rescue_count,
        graph_neutral_count: report.graph_diagnostics.graph_neutral_count,
        total_p95_ms: report.latency.total.p95_ms,
        estimated_usd: report.cost.estimated_usd,
    })
}

async fn drain_external_vector_sync(
    pool: &PgPool,
    config: &MoaConfig,
    limit: i64,
) -> Result<VectorSyncReport> {
    let vector_factory = VectorStoreFactory::from_config(config);
    vector_factory
        .drain_external_sync(pool, limit)
        .await
        .context("drain external vector sync outbox")
}

/// Reprojects a storage partition's stored pgvector embeddings into Turbopuffer.
///
/// The Turbopuffer path seeds `vector_backend = 'turbopuffer'` and reads the
/// tenant's Turbopuffer namespace directly, but that namespace is only populated
/// by draining the incremental `vector_sync_outbox`, which ingestion fills at
/// write time. When a run reuses a tenant whose Postgres partition was already
/// ingested (e.g. a prior `--backend pgvector` run on the same corpus),
/// ingestion skips every record, the outbox is empty, the drain is a no-op, and
/// the Turbopuffer namespace stays empty — retrieval then reads an empty
/// projection and reports near-zero recall. This full reproject copies the
/// partition's stored embeddings into Turbopuffer so the read side mirrors
/// Postgres regardless of ingestion skips. It is an idempotent upsert and reuses
/// the stored embeddings, so it issues no new embedding-provider calls.
async fn reproject_partition_to_turbopuffer(
    pool: &PgPool,
    config: &MoaConfig,
    tenant_id: TenantId,
) -> Result<usize> {
    let vector_factory = VectorStoreFactory::from_config(config);
    let scope = RlsContext::tenant(tenant_id);
    let source: Arc<dyn VectorStore> =
        vector_factory.pgvector_source_for_app_role(pool.clone(), scope);
    let turbopuffer = vector_factory.turbopuffer().context(
        "--backend turbopuffer selected but no Turbopuffer client is configured (set MOA_TURBOPUFFER_API_KEY)",
    )?;
    let target: Arc<dyn VectorStore> = Arc::new(turbopuffer.scoped_to_tenant(tenant_id));
    let promotion = VectorPartitionPromotion::new(pool.clone(), source, target);
    promotion
        .copy_storage_partition(&tenant_id.0.to_string())
        .await
        .context("reproject storage partition into Turbopuffer")
}

async fn build_embedding_export(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
    selected: &SelectedWorkload,
    options: &Options,
    queries: Vec<EmbeddingExportQuery>,
) -> Result<EmbeddingExport> {
    let chunks =
        fetch_embedding_export_chunks(pool, tenant_id, connection_uid, selected, options).await?;
    if chunks.is_empty() {
        bail!(
            "cannot write embedding export for cache `{}` because no active chunk embeddings were found",
            selected.cache_key
        );
    }
    Ok(EmbeddingExport {
        dataset: selected.dataset.as_str().to_string(),
        cache_key: selected.cache_key.clone(),
        embedding_model: options.embedder_name.clone(),
        embedding_dimensions: options.embedding_dim,
        metric: "cosine".to_string(),
        tenant_id: tenant_id.0,
        connection_uid,
        storage_partition_id: tenant_id.0.to_string(),
        chunk_count: chunks.len(),
        query_count: queries.len(),
        chunks,
        queries,
    })
}

async fn fetch_embedding_export_chunks(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
    selected: &SelectedWorkload,
    options: &Options,
) -> Result<Vec<EmbeddingExportChunk>> {
    let scope = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .context("begin scoped embedding export transaction")?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .context("assume moa_app role")?;
    let article_ids = selected
        .articles
        .iter()
        .map(|article| article.id.clone())
        .collect::<Vec<_>>();
    let storage_partition_id = tenant_id.0.to_string();
    let rows = sqlx::query(
        r#"
        SELECT embedding.uid,
               embedding.embedding,
               object.external_object_id AS article_id,
               object.title,
               object.source_uri,
               chunk.text
          FROM moa.embeddings AS embedding
          JOIN moa.knowledge_chunks AS chunk
            ON chunk.storage_partition_id = embedding.storage_partition_id
           AND chunk.graph_node_uid = embedding.uid
          JOIN moa.knowledge_document_versions AS version
            ON version.document_version_uid = chunk.document_version_id
          JOIN moa.knowledge_objects AS object
            ON object.object_uid = version.object_id
         WHERE embedding.storage_partition_id = $1
           AND embedding.label = 'Chunk'
           AND embedding.valid_to IS NULL
           AND object.tenant_id = $2
           AND object.connection_id = $3
           AND object.external_object_id = ANY($4::TEXT[])
           AND object.status = 'active'
         ORDER BY object.external_object_id, chunk.ordinal, chunk.chunk_uid, embedding.uid
        "#,
    )
    .bind(&storage_partition_id)
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(&article_ids)
    .fetch_all(conn.as_mut())
    .await
    .context("fetch active WixQA chunk embeddings for embedding export")?;
    let chunks = rows
        .into_iter()
        .map(|row| {
            let uid = row.try_get("uid")?;
            let embedding: HalfVector = row.try_get("embedding")?;
            let embedding = halfvec_to_f32(embedding);
            ensure_embedding_dimensions("chunk", uid, embedding.len(), options.embedding_dim)?;
            Ok(EmbeddingExportChunk {
                uid,
                article_id: row.try_get("article_id")?,
                title: row.try_get("title")?,
                source_uri: row.try_get("source_uri")?,
                text: row.try_get("text")?,
                embedding,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    conn.commit()
        .await
        .context("commit scoped embedding export transaction")?;
    Ok(chunks)
}

fn halfvec_to_f32(embedding: HalfVector) -> Vec<f32> {
    embedding
        .to_vec()
        .into_iter()
        .map(|value| value.to_f32())
        .collect()
}

fn ensure_embedding_dimensions(
    kind: &str,
    uid: Uuid,
    actual: usize,
    expected: usize,
) -> Result<()> {
    if actual != expected {
        bail!("{kind} embedding {uid} has {actual} dimensions, expected {expected}");
    }
    Ok(())
}

fn write_pretty_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

async fn validate_cached_workload(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
    selected: &SelectedWorkload,
) -> Result<CachedWorkload> {
    let scope = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .context("begin scoped cache validation transaction")?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .context("assume moa_app role")?;
    let article_ids = selected
        .articles
        .iter()
        .map(|article| article.id.clone())
        .collect::<Vec<_>>();
    let (article_count, chunk_count) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT count(DISTINCT object.external_object_id) AS article_count,
               count(DISTINCT chunk.chunk_uid) AS chunk_count
        FROM moa.knowledge_objects AS object
        LEFT JOIN moa.knowledge_document_versions AS version
          ON version.object_id = object.object_uid
        LEFT JOIN moa.knowledge_chunks AS chunk
          ON chunk.document_version_id = version.document_version_uid
         AND chunk.graph_node_uid IS NOT NULL
        WHERE object.tenant_id = $1
          AND object.connection_id = $2
          AND object.external_object_id = ANY($3::TEXT[])
          AND object.status = 'active'
        "#,
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(&article_ids)
    .fetch_one(conn.as_mut())
    .await
    .context("validate cached WixQA workload")?;
    conn.commit()
        .await
        .context("commit cache validation transaction")?;
    Ok(CachedWorkload {
        article_count: u64::try_from(article_count)
            .context("cached article count is non-negative")?,
        chunk_count: u64::try_from(chunk_count).context("cached chunk count is non-negative")?,
    })
}

fn benchmark_config(options: &Options) -> Result<MoaConfig> {
    let mut config = MoaConfig::load_from_env().context("load MOA config from environment")?;
    config.memory.vector.embedder.name = options.embedder_name.clone();
    config.memory.vector.embedder.output_dim = options.embedding_dim;
    config.memory.retrieval.reranker_model = if options.reranker || options.weak_repeat_rerank {
        WIXQA_RERANKER.to_string()
    } else {
        "noop".to_string()
    };
    Ok(config)
}

async fn seed_storage_partition_state(
    pool: &PgPool,
    tenant_id: TenantId,
    storage_partition_id: &str,
    vector_backend: &str,
    embedder: &dyn EmbeddingProvider,
) -> Result<()> {
    let scope = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .context("begin scoped storage partition transaction")?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .context("assume moa_app role")?;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, vector_backend, vector_backend_state, embedding_model,
             embedding_model_version, embedding_dimension, reembed_state)
        VALUES ($1, $2, 'steady', $3, $4, $5, 'steady')
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET vector_backend = EXCLUDED.vector_backend,
                vector_backend_state = EXCLUDED.vector_backend_state,
                embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = EXCLUDED.reembed_state,
                updated_at = now()
        "#,
    )
    .bind(storage_partition_id)
    .bind(vector_backend)
    .bind(embedder.model_id())
    .bind(embedder.model_version())
    .bind(i32::try_from(embedder.dimensions()).context("embedding dimension fits i32")?)
    .execute(conn.as_mut())
    .await
    .context("seed storage partition vector state")?;
    conn.commit()
        .await
        .context("commit storage partition state")?;
    Ok(())
}

async fn ingest_articles(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
    selected: &SelectedWorkload,
    config: &MoaConfig,
    embedder: Arc<dyn EmbeddingProvider>,
) -> Result<IngestionReport> {
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope.clone(),
    ));
    let now = Utc::now();
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: WIXQA_PROVIDER.to_string(),
            connector: WIXQA_CONNECTOR.to_string(),
            provider_account_id: selected.cache_key.clone(),
            credential_ref: "wixqa-local-jsonl".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({
                "dataset": selected.dataset.as_str(),
                "cache_key": selected.cache_key,
                "article_count": selected.articles.len(),
                "question_count": selected.questions.len(),
            }),
            source_selection: json!({}),
            created_at: now,
            updated_at: now,
            last_synced_at: None,
        })
        .await
        .context("upsert WixQA knowledge connection")?;
    let sync_run_uid = Uuid::now_v7();
    let run = KnowledgeSyncRun {
        sync_run_uid,
        tenant_id,
        connection_uid,
        parser: Some("native".to_string()),
        max_records: Some(
            u32::try_from(selected.articles.len()).context("selected article count fits u32")?,
        ),
        status: SyncRunStatus::Ingesting,
        records_seen: 0,
        records_changed: 0,
        records_deleted: 0,
        records_ingested: 0,
        records_failed: 0,
        objects_parsed: 0,
        chunks_embedded: 0,
        graph_nodes_upserted: 0,
        graph_edges_upserted: 0,
        error_code: None,
        started_at: now,
        finished_at: None,
    };
    match repository.claim_sync_run(run).await? {
        SyncRunClaim::Claimed(_) => {}
        SyncRunClaim::AlreadyRunning(existing) => bail!(
            "WixQA connection already has active sync run {} with status {:?}; finish or clear it before rerunning",
            existing.sync_run_uid,
            existing.status
        ),
    }

    let vector_factory = VectorStoreFactory::from_config(config);
    let vector_backend =
        vector_factory.transactional_graph_backend(pool.clone(), scope.clone(), true);
    let graph_store = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope.clone())
        .with_vector_store(vector_backend.vector_store());
    let graph_writer = Arc::new(MemoryKnowledgeGraphWriter::new(
        Arc::new(graph_store),
        MemoryScope::Tenant { tenant_id },
        "wixqa_rag_eval",
    ));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(NativeDocumentParser::new()),
        Arc::new(SharedEmbeddingProvider::new(embedder)),
        graph_writer,
        KnowledgeIngestionPipelineConfig {
            chunking: selected.chunking,
            provider: WIXQA_PROVIDER.to_string(),
            parser_label: "native".to_string(),
        },
    );

    let started = Instant::now();
    let mut page_report = PageIngestionReport::default();
    for batch in selected.articles.chunks(INGESTION_BATCH_SIZE) {
        let records = batch.iter().map(article_to_record).collect::<Vec<_>>();
        let report = pipeline
            .ingest_record_page(
                sync_run_uid,
                connection_uid,
                tenant_id,
                RecordPage {
                    records,
                    next_cursor: None,
                },
            )
            .await
            .with_context(|| format!("ingest WixQA batch ending at {}", batch.len()))?;
        page_report.records_listed = page_report
            .records_listed
            .saturating_add(report.records_listed);
        page_report.records_ingested = page_report
            .records_ingested
            .saturating_add(report.records_ingested);
        page_report.records_skipped = page_report
            .records_skipped
            .saturating_add(report.records_skipped);
        page_report.records_deleted = page_report
            .records_deleted
            .saturating_add(report.records_deleted);
        page_report.embeddings_created = page_report
            .embeddings_created
            .saturating_add(report.embeddings_created);
    }
    complete_sync_run(repository.as_ref(), sync_run_uid).await?;
    Ok(IngestionReport {
        sync_run_uid: Some(sync_run_uid),
        skipped: false,
        cached_articles: None,
        cached_chunks: None,
        records_listed: page_report.records_listed,
        records_ingested: page_report.records_ingested,
        records_skipped: page_report.records_skipped,
        records_deleted: page_report.records_deleted,
        embeddings_created: page_report.embeddings_created,
        elapsed_ms: elapsed_ms(started),
    })
}

async fn complete_sync_run(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
) -> Result<()> {
    let mut run = repository
        .get_sync_run(sync_run_uid)
        .await?
        .with_context(|| format!("sync run {sync_run_uid} disappeared before completion"))?;
    run.status = SyncRunStatus::Completed;
    run.finished_at = Some(Utc::now());
    repository
        .update_sync_run(run)
        .await
        .context("mark WixQA sync run completed")
}

struct RetrievalEvalInputs<'a> {
    pool: &'a PgPool,
    tenant_id: TenantId,
    embedder: Arc<dyn EmbeddingProvider>,
    selected: &'a SelectedWorkload,
    config: &'a MoaConfig,
    top_k: usize,
    use_reranker: bool,
    disable_graph_expansion: bool,
    graph_policy: GraphRetrievalPolicy,
    weak_repeat_fallback_top_k: Option<usize>,
    weak_repeat_rerank: bool,
    capture_embedding_export_queries: bool,
    expected_embedding_dim: usize,
}

async fn retrieve_questions(inputs: RetrievalEvalInputs<'_>) -> Result<QueryRun> {
    let RetrievalEvalInputs {
        pool,
        tenant_id,
        embedder,
        selected,
        config,
        top_k,
        use_reranker,
        disable_graph_expansion,
        graph_policy,
        weak_repeat_fallback_top_k,
        weak_repeat_rerank,
        capture_embedding_export_queries,
        expected_embedding_dim,
    } = inputs;
    let scope = RlsContext::tenant(tenant_id);
    let vector_factory = VectorStoreFactory::from_config(config);
    let pgvector_source = vector_factory.pgvector_source_for_app_role(pool.clone(), scope.clone());
    let graph_store = Arc::new(PostgresGraphStore::scoped_for_app_role(
        pool.clone(),
        scope.clone(),
    ));
    // This harness sizes its own article-level window via --top-k and leaves
    // the request's default (off) `EvidenceWindowPolicy`, so the memory-lane
    // window knobs never clamp it (2026-07-11 MultiHop-RAG recall@10 clamp).
    let retriever =
        HybridRetriever::from_config(config, pool.clone(), graph_store, pgvector_source)
            .with_assume_app_role(true)
            .with_graph_policy(graph_policy);
    let memory_scope = MemoryScope::Tenant { tenant_id };
    let mut measurements = Vec::with_capacity(selected.questions.len());
    let mut embedding_export_queries = Vec::with_capacity(if capture_embedding_export_queries {
        selected.questions.len()
    } else {
        0
    });
    for question in &selected.questions {
        let embed_started = Instant::now();
        let embeddings = embedder
            .embed(std::slice::from_ref(&question.question))
            .await
            .with_context(|| format!("embed WixQA query `{}`", question.question))?;
        let query_embedding = embeddings
            .into_iter()
            .next()
            .context("query embedding provider returned no vectors")?;
        if query_embedding.len() != expected_embedding_dim {
            bail!(
                "query embedding has {} dimensions, expected {}",
                query_embedding.len(),
                expected_embedding_dim
            );
        }
        let export_query = capture_embedding_export_queries.then(|| EmbeddingExportQuery {
            question: question.question.clone(),
            gold_article_ids: question.article_ids.clone(),
            embedding: query_embedding.clone(),
        });
        let query_embedding_ms = elapsed_ms(embed_started);
        let retrieve_started = Instant::now();
        let mut output = retrieve_wixqa_output(
            &retriever,
            &memory_scope,
            question,
            query_embedding.clone(),
            top_k,
            use_reranker,
            disable_graph_expansion,
        )
        .await?;
        let mut effective_top_k = top_k;
        let mut fallback_triggered = false;
        let mut effective_reranker = use_reranker;
        if weak_repeat_fallback_should_run(&output.hits) {
            if let Some(fallback_top_k) = weak_repeat_fallback_top_k {
                output = retrieve_wixqa_output(
                    &retriever,
                    &memory_scope,
                    question,
                    query_embedding.clone(),
                    fallback_top_k,
                    use_reranker,
                    disable_graph_expansion,
                )
                .await?;
                effective_top_k = fallback_top_k;
                fallback_triggered = true;
            } else if weak_repeat_rerank {
                output = retrieve_wixqa_output(
                    &retriever,
                    &memory_scope,
                    question,
                    query_embedding.clone(),
                    top_k,
                    true,
                    disable_graph_expansion,
                )
                .await?;
                fallback_triggered = true;
                effective_reranker = true;
            }
        }
        let retrieval_ms = elapsed_ms(retrieve_started);
        let graph_comparison = if should_compare_graph(output.diagnostics.policy) {
            let comparison_started = Instant::now();
            let graph_off = retrieve_wixqa_output(
                &retriever,
                &memory_scope,
                question,
                query_embedding,
                effective_top_k,
                effective_reranker,
                true,
            )
            .await?;
            let graph_off_retrieval_ms = elapsed_ms(comparison_started);
            Some(graph_comparison_measurement(GraphComparisonInput {
                question,
                url_to_article_id: &selected.url_to_article_id,
                graph_hits: &output.hits,
                graph_off_hits: &graph_off.hits,
                graph_diagnostics: &output.diagnostics,
                graph_off_retrieval_ms,
                effective_top_k,
                graph_off_used_reranker: effective_reranker,
            }))
        } else {
            None
        };
        let ranked_articles = ranked_articles_from_hits(&output.hits, &selected.url_to_article_id);
        let query_metrics = query_metrics(&ranked_articles, &question.article_ids, effective_top_k);
        measurements.push(QueryMeasurement {
            question: question.question.clone(),
            gold_article_ids: question.article_ids.clone(),
            ranked_article_ids: ranked_articles,
            retrieved_hits: output.hits.iter().map(RetrievedHit::from_hit).collect(),
            graph_diagnostics: output.diagnostics,
            graph_comparison,
            metrics: query_metrics,
            effective_top_k,
            fallback_triggered,
            query_embedding_ms,
            retrieval_ms,
            total_ms: query_embedding_ms.saturating_add(retrieval_ms),
        });
        if let Some(export_query) = export_query {
            embedding_export_queries.push(export_query);
        }
    }
    Ok(QueryRun {
        measurements,
        embedding_export_queries,
    })
}

async fn retrieve_wixqa_output(
    retriever: &HybridRetriever,
    memory_scope: &MemoryScope,
    question: &WixQuestion,
    query_embedding: Vec<f32>,
    top_k: usize,
    use_reranker: bool,
    disable_graph_expansion: bool,
) -> Result<RetrievalOutput> {
    retriever
        .retrieve_with_diagnostics(RetrievalRequest {
            seeds: Vec::new(),
            query_text: question.question.clone(),
            query_embedding,
            scope: memory_scope.clone(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            max_pii_class: PiiClass::Restricted,
            k_final: top_k,
            use_reranker,
            strategy: Some(Strategy::VectorFirst),
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion,
            // Article-level retrieval sizes its own window via --top-k; the
            // memory-lane window knobs must not clamp it.
            window_policy: moa_brain::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .with_context(|| format!("retrieve WixQA query `{}`", question.question))
}

fn weak_repeat_fallback_should_run(hits: &[RetrievalHit]) -> bool {
    let mut source_counts = HashMap::new();
    for hit in hits.iter().take(WEAK_REPEAT_FALLBACK_WINDOW) {
        let Some(source_uri) = hit
            .knowledge_chunk
            .as_ref()
            .and_then(|chunk| chunk.source_uri.as_deref())
        else {
            continue;
        };
        let count = source_counts.entry(source_uri).or_insert(0usize);
        *count += 1;
        if *count > WEAK_REPEAT_FALLBACK_THRESHOLD {
            return false;
        }
    }
    !source_counts.is_empty()
}

fn ranked_articles_from_hits(
    hits: &[RetrievalHit],
    url_to_article_id: &HashMap<String, String>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut articles = Vec::new();
    for hit in hits {
        let Some(chunk) = &hit.knowledge_chunk else {
            continue;
        };
        let Some(source_uri) = &chunk.source_uri else {
            continue;
        };
        let Some(article_id) = url_to_article_id.get(source_uri) else {
            continue;
        };
        if seen.insert(article_id.clone()) {
            articles.push(article_id.clone());
        }
    }
    articles
}

fn query_metrics(
    ranked_articles: &[String],
    gold_article_ids: &[String],
    top_k: usize,
) -> QueryMetrics {
    let gold = gold_article_ids.iter().collect::<BTreeSet<_>>();
    let top_source_objects = ranked_articles.iter().take(top_k).collect::<Vec<_>>();
    let matched = top_source_objects
        .iter()
        .filter(|article_id| gold.contains(*article_id))
        .count();
    let hit = matched > 0;
    let recall = if gold_article_ids.is_empty() {
        0.0
    } else {
        matched as f64 / gold_article_ids.len() as f64
    };
    let first_rank = ranked_articles
        .iter()
        .take(top_k)
        .position(|article_id| gold.contains(article_id))
        .map(|index| index + 1);
    let mrr = first_rank.map_or(0.0, |rank| 1.0 / rank as f64);
    let dcg = ranked_articles
        .iter()
        .take(top_k)
        .enumerate()
        .filter(|(_, article_id)| gold.contains(*article_id))
        .map(|(index, _)| 1.0 / ((index + 2) as f64).log2())
        .sum::<f64>();
    let ideal_len = gold_article_ids.len().min(top_k);
    let idcg = (0..ideal_len)
        .map(|index| 1.0 / ((index + 2) as f64).log2())
        .sum::<f64>();
    let ndcg = if idcg > 0.0 { dcg / idcg } else { 0.0 };
    QueryMetrics {
        hit,
        recall,
        mrr,
        ndcg,
        first_relevant_rank: first_rank,
    }
}

fn should_compare_graph(policy: GraphRetrievalPolicy) -> bool {
    !matches!(
        policy,
        GraphRetrievalPolicy::Off | GraphRetrievalPolicy::ContextOnly
    )
}

struct GraphComparisonInput<'a> {
    question: &'a WixQuestion,
    url_to_article_id: &'a HashMap<String, String>,
    graph_hits: &'a [RetrievalHit],
    graph_off_hits: &'a [RetrievalHit],
    graph_diagnostics: &'a GraphRetrievalDiagnostics,
    graph_off_retrieval_ms: u128,
    effective_top_k: usize,
    graph_off_used_reranker: bool,
}

fn graph_comparison_measurement(input: GraphComparisonInput<'_>) -> GraphComparisonMeasurement {
    let GraphComparisonInput {
        question,
        url_to_article_id,
        graph_hits,
        graph_off_hits,
        graph_diagnostics,
        graph_off_retrieval_ms,
        effective_top_k,
        graph_off_used_reranker,
    } = input;
    let graph_ranked_articles = ranked_articles_from_hits(graph_hits, url_to_article_id);
    let graph_off_ranked_article_ids = ranked_articles_from_hits(graph_off_hits, url_to_article_id);
    let graph_metrics = query_metrics(
        &graph_ranked_articles,
        &question.article_ids,
        effective_top_k,
    );
    let graph_off_metrics = query_metrics(
        &graph_off_ranked_article_ids,
        &question.article_ids,
        effective_top_k,
    );
    let impact = classify_graph_impact(
        graph_metrics.first_relevant_rank,
        graph_off_metrics.first_relevant_rank,
        graph_materially_participated(graph_diagnostics, graph_hits),
    );
    GraphComparisonMeasurement {
        impact,
        relevant_rank_with_graph: graph_metrics.first_relevant_rank,
        relevant_rank_without_graph: graph_off_metrics.first_relevant_rank,
        rank_delta_with_minus_without: rank_delta(
            graph_metrics.first_relevant_rank,
            graph_off_metrics.first_relevant_rank,
        ),
        graph_off_ranked_article_ids,
        graph_off_metrics,
        article_rank_movements: article_rank_movements(
            &question.article_ids,
            &graph_ranked_articles,
            graph_off_hits,
            url_to_article_id,
        ),
        top_harmful_graph_paths: if impact == GraphImpact::Hurt {
            top_graph_paths(
                graph_diagnostics,
                graph_hits,
                url_to_article_id,
                &question.article_ids,
            )
        } else {
            Vec::new()
        },
        graph_off_retrieval_ms,
        graph_off_used_reranker,
    }
}

fn classify_graph_impact(
    rank_with_graph: Option<usize>,
    rank_without_graph: Option<usize>,
    graph_participated: bool,
) -> GraphImpact {
    if !graph_participated {
        return GraphImpact::Neutral;
    }
    match rank_order_value(rank_with_graph).cmp(&rank_order_value(rank_without_graph)) {
        std::cmp::Ordering::Less => GraphImpact::Rescue,
        std::cmp::Ordering::Equal => GraphImpact::Neutral,
        std::cmp::Ordering::Greater => GraphImpact::Hurt,
    }
}

fn graph_materially_participated(
    diagnostics: &GraphRetrievalDiagnostics,
    graph_hits: &[RetrievalHit],
) -> bool {
    graph_hits.iter().any(|hit| hit.legs.graph)
        || diagnostics.candidate_counts.graph_only > 0
        || diagnostics.candidate_counts.vector_graph > 0
        || diagnostics.candidate_counts.lexical_graph > 0
        || diagnostics.candidate_counts.all_legs > 0
}

fn rank_delta(rank_with_graph: Option<usize>, rank_without_graph: Option<usize>) -> Option<i64> {
    match (rank_with_graph, rank_without_graph) {
        (Some(with_graph), Some(without_graph)) => Some(with_graph as i64 - without_graph as i64),
        _ => None,
    }
}

fn rank_order_value(rank: Option<usize>) -> usize {
    rank.unwrap_or(usize::MAX)
}

fn article_rank_movements(
    gold_article_ids: &[String],
    graph_ranked_articles: &[String],
    graph_off_hits: &[RetrievalHit],
    url_to_article_id: &HashMap<String, String>,
) -> Vec<ArticleRankMovement> {
    let graph_off_ranked_articles = ranked_articles_from_hits(graph_off_hits, url_to_article_id);
    gold_article_ids
        .iter()
        .map(|article_id| {
            let rank_with_graph = article_rank(graph_ranked_articles, article_id);
            let rank_without_graph = article_rank(&graph_off_ranked_articles, article_id);
            ArticleRankMovement {
                article_id: article_id.clone(),
                rank_with_graph,
                rank_without_graph,
                rank_delta_with_minus_without: rank_delta(rank_with_graph, rank_without_graph),
            }
        })
        .collect()
}

fn article_rank(ranked_articles: &[String], article_id: &str) -> Option<usize> {
    ranked_articles
        .iter()
        .position(|candidate| candidate == article_id)
        .map(|index| index + 1)
}

fn top_graph_paths(
    diagnostics: &GraphRetrievalDiagnostics,
    graph_hits: &[RetrievalHit],
    url_to_article_id: &HashMap<String, String>,
    gold_article_ids: &[String],
) -> Vec<GraphPathDiagnostic> {
    let hit_contexts = graph_hit_contexts(graph_hits, url_to_article_id);
    let gold = gold_article_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let harmful_candidate_uids = hit_contexts
        .iter()
        .filter_map(|(uid, context)| {
            let is_gold = context
                .article_id
                .as_ref()
                .is_some_and(|article_id| gold.contains(article_id.as_str()));
            (!is_gold).then_some(*uid)
        })
        .collect::<HashSet<_>>();
    let graph_candidate_uids = hit_contexts.keys().copied().collect::<HashSet<_>>();
    let mut paths = graph_path_diagnostics_for_candidates(
        &diagnostics.path_traces,
        &hit_contexts,
        &harmful_candidate_uids,
    );
    if paths.is_empty() {
        paths = graph_path_diagnostics_for_candidates(
            &diagnostics.path_traces,
            &hit_contexts,
            &graph_candidate_uids,
        );
    }
    if paths.is_empty() {
        paths = diagnostics
            .path_traces
            .iter()
            .map(|trace| graph_path_diagnostic(trace, None))
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
            .then_with(|| left.edge_directions.cmp(&right.edge_directions))
    });
    paths.truncate(5);
    paths
}

fn graph_hit_contexts(
    hits: &[RetrievalHit],
    url_to_article_id: &HashMap<String, String>,
) -> HashMap<Uuid, GraphHitContext> {
    let mut contexts = HashMap::new();
    for (index, hit) in hits.iter().enumerate() {
        if !hit.legs.graph {
            continue;
        }
        let article_id = hit
            .knowledge_chunk
            .as_ref()
            .and_then(|chunk| chunk.source_uri.as_ref())
            .and_then(|source_uri| url_to_article_id.get(source_uri))
            .cloned();
        contexts.entry(hit.uid).or_insert(GraphHitContext {
            rank: index + 1,
            article_id,
        });
    }
    contexts
}

fn graph_path_diagnostics_for_candidates(
    traces: &[GraphPathTrace],
    hit_contexts: &HashMap<Uuid, GraphHitContext>,
    candidate_uids: &HashSet<Uuid>,
) -> Vec<GraphPathDiagnostic> {
    traces
        .iter()
        .filter(|trace| candidate_uids.contains(&trace.candidate_uid))
        .map(|trace| graph_path_diagnostic(trace, hit_contexts.get(&trace.candidate_uid)))
        .collect()
}

fn graph_path_diagnostic(
    trace: &GraphPathTrace,
    context: Option<&GraphHitContext>,
) -> GraphPathDiagnostic {
    GraphPathDiagnostic {
        seed_uid: trace.seed_uid,
        seed_source: trace.seed_source,
        candidate_uid: trace.candidate_uid,
        candidate_rank_with_graph: context.map(|context| context.rank),
        candidate_article_id: context.and_then(|context| context.article_id.clone()),
        hop: trace.hop,
        edge_labels: trace.edge_labels.clone(),
        edge_directions: trace.edge_directions.clone(),
    }
}

fn build_report(
    options: &Options,
    tenant_id: TenantId,
    connection_uid: Uuid,
    selected: SelectedWorkload,
    ingestion: IngestionReport,
    vector_sync: VectorSyncReport,
    measurements: Vec<QueryMeasurement>,
) -> WixQaReport {
    let aggregate = aggregate_metrics(&measurements);
    let latency = latency_report(&measurements);
    let cost = cost_report(&selected, &measurements, options);
    let fallback = FallbackReport::from_options(options, &measurements);
    let graph_diagnostics = graph_diagnostics_report(&measurements, options);
    WixQaReport {
        generated_at: Utc::now().to_rfc3339(),
        dataset: selected.dataset.as_str().to_string(),
        backend: options.backend.as_str().to_string(),
        reranker: if options.reranker || options.weak_repeat_rerank {
            WIXQA_RERANKER.to_string()
        } else {
            "noop".to_string()
        },
        embedding_model: options.embedder_name.clone(),
        embedding_dimensions: options.embedding_dim,
        tenant_id: tenant_id.0,
        connection_uid,
        cache_key: selected.cache_key,
        question_count: selected.questions.len(),
        article_count: selected.articles.len(),
        top_k: options.top_k,
        question_offset: options.question_offset,
        question_limit: options.question_limit,
        max_articles: options.max_articles,
        disable_graph_expansion: options.disable_graph_expansion,
        graph_policy: graph_diagnostics.policy,
        graph_diagnostics,
        chunking: ChunkingReport::from(selected.chunking),
        ingestion,
        vector_sync: VectorSyncReportJson::from(vector_sync),
        turbopuffer_reprojected: None,
        fallback,
        metrics: aggregate,
        latency,
        cost,
        per_query: measurements,
        notes: vec![
            "WixQA gold uses article_ids; this report measures source-object retrieval before answer generation.".to_string(),
            "Rerank cost is estimated per query when reranker is enabled; embedding token counts use the repo's chars/4 estimator.".to_string(),
        ],
    }
}

fn aggregate_metrics(measurements: &[QueryMeasurement]) -> AggregateMetrics {
    if measurements.is_empty() {
        return AggregateMetrics::default();
    }
    let count = measurements.len() as f64;
    AggregateMetrics {
        hit_at_k: measurements
            .iter()
            .filter(|measurement| measurement.metrics.hit)
            .count() as f64
            / count,
        recall_at_k: measurements
            .iter()
            .map(|measurement| measurement.metrics.recall)
            .sum::<f64>()
            / count,
        mrr: measurements
            .iter()
            .map(|measurement| measurement.metrics.mrr)
            .sum::<f64>()
            / count,
        ndcg_at_k: measurements
            .iter()
            .map(|measurement| measurement.metrics.ndcg)
            .sum::<f64>()
            / count,
    }
}

fn graph_diagnostics_report(
    measurements: &[QueryMeasurement],
    options: &Options,
) -> GraphDiagnosticsReport {
    let mut report = GraphDiagnosticsReport {
        policy: if options.disable_graph_expansion {
            GraphRetrievalPolicy::Off
        } else {
            options.graph_policy
        },
        ..GraphDiagnosticsReport::default()
    };
    let mut graph_latencies = Vec::with_capacity(measurements.len());
    for measurement in measurements {
        let diagnostics = &measurement.graph_diagnostics;
        report.seed_counts.planner += diagnostics.seed_counts.planner;
        report.seed_counts.exact_phase_one += diagnostics.seed_counts.exact_phase_one;
        report.seed_counts.broad_fallback += diagnostics.seed_counts.broad_fallback;
        report.seed_counts.semantic_entity += diagnostics.seed_counts.semantic_entity;
        for (label, count) in &diagnostics.path_label_histogram {
            *report
                .path_label_histogram
                .entry(label.clone())
                .or_default() += count;
        }
        for (hop, count) in &diagnostics.hop_histogram {
            *report.hop_histogram.entry(*hop).or_default() += count;
        }
        report.candidate_counts.add(diagnostics.candidate_counts);
        report.raw_path_count += diagnostics.raw_path_count;
        report
            .source_object_ranking
            .record(&diagnostics.source_object_ranking);
        graph_latencies.push(u128::from(diagnostics.graph_latency_ms));
        if let Some(comparison) = &measurement.graph_comparison {
            report.compared_query_count += 1;
            match comparison.impact {
                GraphImpact::Hurt => report.graph_hurt_count += 1,
                GraphImpact::Rescue => report.graph_rescue_count += 1,
                GraphImpact::Neutral => report.graph_neutral_count += 1,
            }
        }
    }
    report.graph_latency = LatencySummary::from_values(graph_latencies);
    report.source_object_ranking.finish();
    report
}

fn latency_report(measurements: &[QueryMeasurement]) -> LatencyReport {
    LatencyReport {
        query_embedding: LatencySummary::from_values(
            measurements
                .iter()
                .map(|measurement| measurement.query_embedding_ms)
                .collect(),
        ),
        retrieval: LatencySummary::from_values(
            measurements
                .iter()
                .map(|measurement| measurement.retrieval_ms)
                .collect(),
        ),
        total: LatencySummary::from_values(
            measurements
                .iter()
                .map(|measurement| measurement.total_ms)
                .collect(),
        ),
    }
}

fn cost_report(
    selected: &SelectedWorkload,
    measurements: &[QueryMeasurement],
    options: &Options,
) -> CostReport {
    let doc_tokens = selected
        .articles
        .iter()
        .map(|article| estimate_tokens(&article.contents))
        .sum::<u64>();
    let query_tokens = selected
        .questions
        .iter()
        .map(|question| estimate_tokens(&question.question))
        .sum::<u64>();
    let fallback_calls = measurements
        .iter()
        .filter(|measurement| measurement.fallback_triggered)
        .count();
    let rerank_calls = if options.reranker {
        u64::try_from(measurements.len().saturating_add(fallback_calls)).unwrap_or(u64::MAX)
    } else if options.weak_repeat_rerank {
        u64::try_from(fallback_calls).unwrap_or(u64::MAX)
    } else {
        0
    }
    .saturating_add(
        u64::try_from(
            measurements
                .iter()
                .filter(|measurement| {
                    measurement
                        .graph_comparison
                        .as_ref()
                        .is_some_and(|comparison| comparison.graph_off_used_reranker)
                })
                .count(),
        )
        .unwrap_or(u64::MAX),
    );
    let embedding_input_usd_per_million_tokens =
        embedding_input_usd_per_million_tokens(&options.embedder_name);
    let embedding_usd = usd_per_million(
        doc_tokens.saturating_add(query_tokens),
        embedding_input_usd_per_million_tokens,
    );
    let rerank_usd = rerank_calls as f64 * COHERE_RERANK_V4_FAST_USD_PER_SEARCH;
    CostReport {
        pricing_as_of: PRICING_AS_OF.to_string(),
        estimated_document_embed_tokens: doc_tokens,
        estimated_query_embed_tokens: query_tokens,
        embedding_input_usd_per_million_tokens,
        rerank_calls,
        estimated_usd: embedding_usd + rerank_usd,
    }
}

fn embedding_input_usd_per_million_tokens(embedder_name: &str) -> f64 {
    match embedder_name {
        WIXQA_GEMINI_EMBEDDER => GEMINI_EMBEDDING_2_INPUT_USD_PER_MILLION_TOKENS,
        _ => COHERE_EMBED_V4_INPUT_USD_PER_MILLION_TOKENS,
    }
}

fn usd_per_million(tokens: u64, usd_per_million_tokens: f64) -> f64 {
    tokens as f64 * usd_per_million_tokens / 1_000_000.0
}

fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count();
    u64::try_from(chars.max(1).div_ceil(4)).unwrap_or(u64::MAX)
}

fn article_to_record(article: &WixArticle) -> ProviderRecord {
    ProviderRecord {
        source_id: article.id.clone(),
        object_type: article.article_type.clone(),
        title: Some(article.title.clone()),
        source_uri: Some(article.url.clone()),
        change_token: Some(content_hash(&article.contents)),
        deleted: false,
        source_updated_at: None,
        metadata: json!({
            "wixqa_article_type": article.article_type,
            "wixqa_id": article.id,
        }),
        payload: json!({
            "content": article.contents,
            "mime_type": "text/plain",
        }),
    }
}

fn load_corpus(data_dir: &Path, dataset: Dataset) -> Result<Vec<WixArticle>> {
    let path = data_dir.join(dataset.corpus_path());
    read_jsonl(&path).with_context(|| format!("load KB corpus {}", path.display()))
}

fn load_questions(data_dir: &Path, dataset: Dataset) -> Result<Vec<WixQuestion>> {
    let path = data_dir.join(dataset.question_path());
    read_jsonl(&path).with_context(|| format!("load WixQA questions {}", path.display()))
}

fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    raw.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("parse {} line {}", path.display(), index + 1))
        })
        .collect()
}

fn select_workload(
    corpus: &[WixArticle],
    questions: &[WixQuestion],
    options: &Options,
) -> Result<SelectedWorkload> {
    if options.top_k == 0 {
        bail!("--top-k must be greater than 0");
    }
    let selected_questions = questions
        .iter()
        .skip(options.question_offset)
        .take(options.question_limit.unwrap_or(questions.len()))
        .cloned()
        .collect::<Vec<_>>();
    if selected_questions.is_empty() {
        bail!("selected WixQA question set is empty");
    }
    let corpus_by_id = corpus
        .iter()
        .map(|article| (article.id.clone(), article.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut selected_ids = BTreeSet::new();
    for question in &selected_questions {
        for article_id in &question.article_ids {
            if !corpus_by_id.contains_key(article_id) {
                bail!(
                    "question references article_id `{}` that is absent from KB corpus",
                    article_id
                );
            }
            selected_ids.insert(article_id.clone());
        }
    }
    if let Some(max_articles) = options.max_articles {
        if max_articles < selected_ids.len() {
            bail!(
                "--max-articles={} is smaller than the {} gold articles required by selected questions",
                max_articles,
                selected_ids.len()
            );
        }
        for article_id in corpus_by_id.keys() {
            if selected_ids.len() >= max_articles {
                break;
            }
            selected_ids.insert(article_id.clone());
        }
    } else {
        selected_ids.extend(corpus_by_id.keys().cloned());
    }
    let articles = selected_ids
        .iter()
        .map(|article_id| {
            corpus_by_id
                .get(article_id)
                .cloned()
                .with_context(|| format!("selected article `{article_id}` disappeared"))
        })
        .collect::<Result<Vec<_>>>()?;
    let url_to_article_id = articles
        .iter()
        .map(|article| (article.url.clone(), article.id.clone()))
        .collect::<HashMap<_, _>>();
    let chunking = ChunkingConfig {
        target_tokens: options.chunk_target_tokens,
        max_tokens: options.chunk_max_tokens,
        min_tokens: options.chunk_min_tokens,
    };
    let cache_key = options
        .cache_key
        .clone()
        .unwrap_or_else(|| workload_cache_key(options, &selected_questions, &articles, chunking));
    Ok(SelectedWorkload {
        dataset: options.dataset,
        questions: selected_questions,
        articles,
        url_to_article_id,
        chunking,
        cache_key,
    })
}

fn workload_cache_key(
    options: &Options,
    questions: &[WixQuestion],
    articles: &[WixArticle],
    chunking: ChunkingConfig,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(options.dataset.as_str().as_bytes());
    hasher.update(&options.question_offset.to_le_bytes());
    hasher.update(&options.question_limit.unwrap_or(usize::MAX).to_le_bytes());
    hasher.update(&options.max_articles.unwrap_or(usize::MAX).to_le_bytes());
    hasher.update(&chunking.target_tokens.to_le_bytes());
    hasher.update(&chunking.max_tokens.to_le_bytes());
    hasher.update(&chunking.min_tokens.to_le_bytes());
    hasher.update(options.embedder_name.as_bytes());
    hasher.update(&options.embedding_dim.to_le_bytes());
    for question in questions {
        hasher.update(question.question.as_bytes());
        for article_id in &question.article_ids {
            hasher.update(article_id.as_bytes());
        }
    }
    for article in articles {
        hasher.update(article.id.as_bytes());
        hasher.update(content_hash(&article.contents).as_bytes());
    }
    format!("wixqa:{}", hasher.finalize().to_hex())
}

#[derive(Clone)]
struct SharedEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
}

impl SharedEmbeddingProvider {
    fn new(inner: Arc<dyn EmbeddingProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl EmbeddingProvider for SharedEmbeddingProvider {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_version(&self) -> i32 {
        self.inner.model_version()
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        self.inner.embed(inputs).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dataset {
    ExpertWritten,
    Simulated,
    Synthetic,
    /// MultiHop-RAG (yixuantt, COLM 2024): news queries whose gold evidence
    /// spans 2-4 of 609 articles — the external relationship-based retrieval
    /// lane from the 2026-07-11 RAG accuracy plan. Fetch and convert with
    /// `scripts/fetch_multihoprag.py`.
    MultihopRag,
    /// FinanceBench (PatronusAI): 150 SEC-filing questions over an
    /// evidence-snippet corpus (84 documents) — the financial external lane.
    /// Fetch and convert with `scripts/fetch_financebench.py`.
    Financebench,
}

impl Dataset {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "expertwritten" | "expert" | "wixqa_expertwritten" => Ok(Self::ExpertWritten),
            "simulated" | "wixqa_simulated" => Ok(Self::Simulated),
            "synthetic" | "wixqa_synthetic" => Ok(Self::Synthetic),
            "multihoprag" | "multihop" => Ok(Self::MultihopRag),
            "financebench" | "finbench" => Ok(Self::Financebench),
            other => bail!("unknown WixQA dataset `{other}`"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ExpertWritten => "expertwritten",
            Self::Simulated => "simulated",
            Self::Synthetic => "synthetic",
            Self::MultihopRag => "multihoprag",
            Self::Financebench => "financebench",
        }
    }

    fn question_path(self) -> &'static str {
        match self {
            Self::ExpertWritten => "wixqa_expertwritten/test.jsonl",
            Self::Simulated => "wixqa_simulated/test.jsonl",
            Self::Synthetic => "wixqa_synthetic/test.jsonl",
            Self::MultihopRag => "multihoprag/questions.jsonl",
            Self::Financebench => "financebench/questions.jsonl",
        }
    }

    fn corpus_path(self) -> &'static str {
        match self {
            Self::ExpertWritten | Self::Simulated | Self::Synthetic => {
                "wix_kb_corpus/wix_kb_corpus.jsonl"
            }
            Self::MultihopRag => "multihoprag/corpus.jsonl",
            Self::Financebench => "financebench/corpus.jsonl",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Pgvector,
    Turbopuffer,
}

impl Backend {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "pgvector" => Ok(Self::Pgvector),
            "turbopuffer" => Ok(Self::Turbopuffer),
            other => bail!("unknown vector backend `{other}`"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pgvector => "pgvector",
            Self::Turbopuffer => "turbopuffer",
        }
    }
}

#[derive(Debug)]
struct Options {
    data_dir: PathBuf,
    output: PathBuf,
    dataset: Dataset,
    backend: Backend,
    question_limit: Option<usize>,
    question_offset: usize,
    max_articles: Option<usize>,
    top_k: usize,
    embedder_name: String,
    embedding_dim: usize,
    reranker: bool,
    disable_graph_expansion: bool,
    graph_policy: GraphRetrievalPolicy,
    chunk_target_tokens: usize,
    chunk_max_tokens: usize,
    chunk_min_tokens: usize,
    cache_key: Option<String>,
    skip_ingestion: bool,
    drain_vector_sync: bool,
    vector_sync_drain_limit: i64,
    weak_repeat_fallback_top_k: Option<usize>,
    weak_repeat_rerank: bool,
    embedding_export: Option<PathBuf>,
    help: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            output: PathBuf::from(DEFAULT_OUTPUT),
            dataset: Dataset::Simulated,
            backend: Backend::Pgvector,
            question_limit: Some(20),
            question_offset: 0,
            max_articles: Some(300),
            top_k: 10,
            embedder_name: WIXQA_DEFAULT_EMBEDDER.to_string(),
            embedding_dim: WIXQA_DEFAULT_EMBEDDING_DIMENSIONS,
            reranker: false,
            disable_graph_expansion: false,
            graph_policy: GraphRetrievalPolicy::default(),
            chunk_target_tokens: ChunkingConfig::default().target_tokens,
            chunk_max_tokens: ChunkingConfig::default().max_tokens,
            chunk_min_tokens: ChunkingConfig::default().min_tokens,
            cache_key: None,
            skip_ingestion: false,
            drain_vector_sync: false,
            vector_sync_drain_limit: VECTOR_SYNC_DRAIN_LIMIT,
            weak_repeat_fallback_top_k: None,
            weak_repeat_rerank: false,
            embedding_export: None,
            help: false,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => options.help = true,
                "--data-dir" => options.data_dir = PathBuf::from(required_value(&mut args, &arg)?),
                "--output" => options.output = PathBuf::from(required_value(&mut args, &arg)?),
                "--dataset" => {
                    options.dataset = Dataset::parse(&required_value(&mut args, &arg)?)?;
                }
                "--backend" => {
                    options.backend = Backend::parse(&required_value(&mut args, &arg)?)?;
                }
                "--question-limit" => {
                    let value = required_value(&mut args, &arg)?;
                    options.question_limit = if value == "all" {
                        None
                    } else {
                        Some(parse_usize(&arg, &value)?)
                    };
                }
                "--question-offset" => {
                    options.question_offset = parse_usize(&arg, &required_value(&mut args, &arg)?)?;
                }
                "--max-articles" => {
                    let value = required_value(&mut args, &arg)?;
                    options.max_articles = if value == "all" {
                        None
                    } else {
                        Some(parse_usize(&arg, &value)?)
                    };
                }
                "--top-k" => options.top_k = parse_usize(&arg, &required_value(&mut args, &arg)?)?,
                "--embedder-name" => {
                    options.embedder_name = required_value(&mut args, &arg)?;
                }
                "--embedding-dim" => {
                    options.embedding_dim = parse_usize(&arg, &required_value(&mut args, &arg)?)?;
                }
                "--reranker" => {
                    options.reranker = parse_bool_flag(&arg, args.peek().map(String::as_str))?;
                    if args
                        .peek()
                        .is_some_and(|value| value == "on" || value == "off")
                    {
                        args.next();
                    }
                }
                "--disable-graph-expansion" => options.disable_graph_expansion = true,
                "--graph-policy" => {
                    options.graph_policy = parse_graph_policy(&required_value(&mut args, &arg)?)?;
                }
                "--chunk-target-tokens" => {
                    options.chunk_target_tokens =
                        parse_usize(&arg, &required_value(&mut args, &arg)?)?;
                }
                "--chunk-max-tokens" => {
                    options.chunk_max_tokens =
                        parse_usize(&arg, &required_value(&mut args, &arg)?)?;
                }
                "--chunk-min-tokens" => {
                    options.chunk_min_tokens =
                        parse_usize(&arg, &required_value(&mut args, &arg)?)?;
                }
                "--cache-key" => {
                    options.cache_key = Some(required_value(&mut args, &arg)?);
                }
                "--skip-ingestion" => options.skip_ingestion = true,
                "--drain-vector-sync" => options.drain_vector_sync = true,
                "--vector-sync-drain-limit" => {
                    options.vector_sync_drain_limit =
                        parse_i64(&arg, &required_value(&mut args, &arg)?)?;
                }
                "--weak-repeat-fallback-top-k" => {
                    options.weak_repeat_fallback_top_k =
                        Some(parse_usize(&arg, &required_value(&mut args, &arg)?)?);
                }
                "--weak-repeat-rerank" => options.weak_repeat_rerank = true,
                "--embedding-export" => {
                    options.embedding_export =
                        Some(PathBuf::from(required_value(&mut args, &arg)?));
                }
                other => bail!("unknown wixqa-rag-eval option: {other}"),
            }
        }
        if options.chunk_min_tokens > options.chunk_target_tokens {
            bail!("--chunk-min-tokens must be <= --chunk-target-tokens");
        }
        if options.chunk_target_tokens > options.chunk_max_tokens {
            bail!("--chunk-target-tokens must be <= --chunk-max-tokens");
        }
        if options.vector_sync_drain_limit <= 0 {
            bail!("--vector-sync-drain-limit must be greater than 0");
        }
        if options.embedding_dim == 0 {
            bail!("--embedding-dim must be greater than 0");
        }
        validate_embedder_name(&options.embedder_name)?;
        if let Some(fallback_top_k) = options.weak_repeat_fallback_top_k
            && fallback_top_k <= options.top_k
        {
            bail!("--weak-repeat-fallback-top-k must be greater than --top-k");
        }
        if options.weak_repeat_rerank && options.reranker {
            bail!("--weak-repeat-rerank cannot be combined with --reranker");
        }
        if options.weak_repeat_rerank && options.weak_repeat_fallback_top_k.is_some() {
            bail!("--weak-repeat-rerank cannot be combined with --weak-repeat-fallback-top-k");
        }
        Ok(options)
    }
}

fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{flag} requires a value"))
}

fn parse_usize(flag: &str, value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .with_context(|| format!("{flag} expects a non-negative integer, got `{value}`"))
}

fn parse_i64(flag: &str, value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .with_context(|| format!("{flag} expects an integer, got `{value}`"))
}

fn validate_embedder_name(value: &str) -> Result<()> {
    match value {
        WIXQA_DEFAULT_EMBEDDER | WIXQA_GEMINI_EMBEDDER => Ok(()),
        other => bail!(
            "--embedder-name must be `{}` or `{}`, got `{}`",
            WIXQA_DEFAULT_EMBEDDER,
            WIXQA_GEMINI_EMBEDDER,
            other
        ),
    }
}

fn parse_bool_flag(flag: &str, next: Option<&str>) -> Result<bool> {
    match next {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(value) if value.starts_with('-') => Ok(true),
        None => Ok(true),
        Some(value) => bail!("{flag} expects optional `on` or `off`, got `{value}`"),
    }
}

fn parse_graph_policy(value: &str) -> Result<GraphRetrievalPolicy> {
    GraphRetrievalPolicy::from_str_label(value).with_context(|| {
        format!(
            "unsupported --graph-policy value `{value}`; expected off|context-only|legacy-broad-expansion|anchored-rescue|source-graph|entity-local-search|propagation|community"
        )
    })
}

fn print_help() {
    println!(
        "Usage: cargo run -p xtask --features eval-tools -- wixqa-rag-eval [options]\n\
         \n\
         Options:\n\
           --data-dir PATH              WixQA raw JSONL cache (default: {DEFAULT_DATA_DIR})\n\
           --output PATH                Report JSON path (default: {DEFAULT_OUTPUT})\n\
           --dataset NAME               simulated|expertwritten|synthetic|multihoprag|financebench (default: simulated)\n\
           --backend NAME               pgvector|turbopuffer (default: pgvector)\n\
           --question-limit N|all       Questions to evaluate (default: 20)\n\
           --question-offset N          Offset into the selected QA split (default: 0)\n\
          --max-articles N|all         Gold articles plus deterministic distractors (default: 300)\n\
          --top-k N                    Article-level retrieval cutoff (default: 10)\n\
          --embedder-name NAME         cohere:embed-v4.0|gemini:gemini-embedding-2 (default: cohere:embed-v4.0)\n\
          --embedding-dim N            Embedding output dimension (default: 1024)\n\
          --reranker [on|off]          Enable Cohere rerank-v4.0-fast (default: off)\n\
           --disable-graph-expansion    Measure vector+lexical fusion without graph expansion\n\
          --graph-policy NAME          off|context-only|legacy-broad-expansion|anchored-rescue|source-graph|entity-local-search|propagation|community (default: anchored-rescue)\n\
           --chunk-target-tokens N      Chunk target tokens (default: 700)\n\
           --chunk-max-tokens N         Chunk max tokens (default: 1000)\n\
           --chunk-min-tokens N         Chunk min tokens (default: 120)\n\
           --cache-key VALUE            Override deterministic tenant/index key for corpus reuse\n\
           --skip-ingestion             Reuse an existing cache and run retrieval only\n\
           --drain-vector-sync          Drain external vector sync even when ingestion is skipped\n\
          --vector-sync-drain-limit N  Maximum outbox rows to drain (default: 100000)\n\
          --weak-repeat-fallback-top-k N  Rerun retrieval at N when top-10 has no repeated source article\n\
          --weak-repeat-rerank       Rerank only weak-repeat queries at the primary top-k\n\
          --embedding-export PATH        Write eval-only chunk/query embedding bundle\n"
    );
}

#[derive(Debug, Clone, Deserialize)]
struct WixArticle {
    id: String,
    url: String,
    contents: String,
    title: String,
    article_type: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WixQuestion {
    question: String,
    article_ids: Vec<String>,
}

struct SelectedWorkload {
    dataset: Dataset,
    questions: Vec<WixQuestion>,
    articles: Vec<WixArticle>,
    url_to_article_id: HashMap<String, String>,
    chunking: ChunkingConfig,
    cache_key: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct CachedWorkload {
    article_count: u64,
    chunk_count: u64,
}

#[derive(Debug, Clone, Serialize)]
struct EmbeddingExport {
    dataset: String,
    cache_key: String,
    embedding_model: String,
    embedding_dimensions: usize,
    metric: String,
    tenant_id: Uuid,
    connection_uid: Uuid,
    storage_partition_id: String,
    chunk_count: usize,
    query_count: usize,
    chunks: Vec<EmbeddingExportChunk>,
    queries: Vec<EmbeddingExportQuery>,
}

#[derive(Debug, Clone, Serialize)]
struct EmbeddingExportChunk {
    uid: Uuid,
    article_id: String,
    title: Option<String>,
    source_uri: Option<String>,
    text: String,
    embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize)]
struct EmbeddingExportQuery {
    question: String,
    gold_article_ids: Vec<String>,
    embedding: Vec<f32>,
}

#[derive(Debug)]
struct QueryRun {
    measurements: Vec<QueryMeasurement>,
    embedding_export_queries: Vec<EmbeddingExportQuery>,
}

#[derive(Debug, Clone, Serialize)]
struct WixQaReport {
    generated_at: String,
    dataset: String,
    backend: String,
    reranker: String,
    embedding_model: String,
    embedding_dimensions: usize,
    tenant_id: Uuid,
    connection_uid: Uuid,
    cache_key: String,
    question_count: usize,
    article_count: usize,
    top_k: usize,
    question_offset: usize,
    question_limit: Option<usize>,
    max_articles: Option<usize>,
    disable_graph_expansion: bool,
    graph_policy: GraphRetrievalPolicy,
    graph_diagnostics: GraphDiagnosticsReport,
    chunking: ChunkingReport,
    ingestion: IngestionReport,
    vector_sync: VectorSyncReportJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    turbopuffer_reprojected: Option<u64>,
    fallback: FallbackReport,
    metrics: AggregateMetrics,
    latency: LatencyReport,
    cost: CostReport,
    per_query: Vec<QueryMeasurement>,
    notes: Vec<String>,
}

#[derive(Debug)]
struct RunSummary {
    output: PathBuf,
    dataset: String,
    backend: String,
    question_count: usize,
    article_count: usize,
    metric_cutoff: String,
    recall_at_k: f64,
    hit_at_k: f64,
    mrr: f64,
    ndcg_at_k: f64,
    graph_hurt_count: usize,
    graph_rescue_count: usize,
    graph_neutral_count: usize,
    total_p95_ms: u128,
    estimated_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
struct IngestionReport {
    sync_run_uid: Option<Uuid>,
    skipped: bool,
    cached_articles: Option<u64>,
    cached_chunks: Option<u64>,
    records_listed: u64,
    records_ingested: u64,
    records_skipped: u64,
    records_deleted: u64,
    embeddings_created: u64,
    elapsed_ms: u128,
}

impl IngestionReport {
    fn skipped(cached: CachedWorkload) -> Self {
        Self {
            sync_run_uid: None,
            skipped: true,
            cached_articles: Some(cached.article_count),
            cached_chunks: Some(cached.chunk_count),
            records_listed: 0,
            records_ingested: 0,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            elapsed_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct VectorSyncReportJson {
    attempted: u64,
    succeeded: u64,
    failed: u64,
    skipped: u64,
}

impl From<VectorSyncReport> for VectorSyncReportJson {
    fn from(report: VectorSyncReport) -> Self {
        Self {
            attempted: report.attempted,
            succeeded: report.succeeded,
            failed: report.failed,
            skipped: report.skipped,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct AggregateMetrics {
    hit_at_k: f64,
    recall_at_k: f64,
    mrr: f64,
    ndcg_at_k: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
struct GraphDiagnosticsReport {
    policy: GraphRetrievalPolicy,
    seed_counts: GraphSeedDiagnostics,
    path_label_histogram: BTreeMap<String, usize>,
    hop_histogram: BTreeMap<u8, usize>,
    candidate_counts: GraphCandidateCounts,
    raw_path_count: usize,
    source_object_ranking: SourceObjectRankingReport,
    graph_latency: LatencySummary,
    graph_hurt_count: usize,
    graph_rescue_count: usize,
    graph_neutral_count: usize,
    compared_query_count: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
struct SourceObjectRankingReport {
    enabled_query_count: usize,
    ranked_source_object_count: usize,
    feature_totals: SourceObjectFeatureContributions,
    top_rank_movements: Vec<SourceObjectFeatureContribution>,
}

impl SourceObjectRankingReport {
    fn record(&mut self, diagnostics: &moa_brain::retrieval::SourceObjectRankingDiagnostics) {
        if !diagnostics.enabled {
            return;
        }
        self.enabled_query_count += 1;
        self.ranked_source_object_count += diagnostics.ranked_source_object_count;
        self.feature_totals.add(diagnostics.feature_totals);
        self.top_rank_movements
            .extend(diagnostics.top_source_objects.iter().cloned());
    }

    fn finish(&mut self) {
        self.top_rank_movements.sort_by(|left, right| {
            movement_magnitude(right)
                .cmp(&movement_magnitude(left))
                .then_with(|| right.score.total_cmp(&left.score))
                .then_with(|| left.object_uid.cmp(&right.object_uid))
        });
        self.top_rank_movements.truncate(20);
    }
}

fn movement_magnitude(contribution: &SourceObjectFeatureContribution) -> usize {
    contribution
        .rank_delta_after_minus_before
        .map(i64::unsigned_abs)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
struct QueryMeasurement {
    question: String,
    gold_article_ids: Vec<String>,
    ranked_article_ids: Vec<String>,
    retrieved_hits: Vec<RetrievedHit>,
    graph_diagnostics: GraphRetrievalDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_comparison: Option<GraphComparisonMeasurement>,
    metrics: QueryMetrics,
    effective_top_k: usize,
    fallback_triggered: bool,
    query_embedding_ms: u128,
    retrieval_ms: u128,
    total_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
struct GraphComparisonMeasurement {
    impact: GraphImpact,
    relevant_rank_with_graph: Option<usize>,
    relevant_rank_without_graph: Option<usize>,
    rank_delta_with_minus_without: Option<i64>,
    graph_off_ranked_article_ids: Vec<String>,
    graph_off_metrics: QueryMetrics,
    article_rank_movements: Vec<ArticleRankMovement>,
    top_harmful_graph_paths: Vec<GraphPathDiagnostic>,
    graph_off_retrieval_ms: u128,
    graph_off_used_reranker: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GraphImpact {
    Hurt,
    Rescue,
    Neutral,
}

#[derive(Debug, Clone, Serialize)]
struct ArticleRankMovement {
    article_id: String,
    rank_with_graph: Option<usize>,
    rank_without_graph: Option<usize>,
    rank_delta_with_minus_without: Option<i64>,
}

#[derive(Debug, Clone)]
struct GraphHitContext {
    rank: usize,
    article_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GraphPathDiagnostic {
    seed_uid: Uuid,
    seed_source: Option<GraphSeedSource>,
    candidate_uid: Uuid,
    candidate_rank_with_graph: Option<usize>,
    candidate_article_id: Option<String>,
    hop: u8,
    edge_labels: Vec<String>,
    edge_directions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct QueryMetrics {
    hit: bool,
    recall: f64,
    mrr: f64,
    ndcg: f64,
    first_relevant_rank: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct FallbackReport {
    strategy: String,
    primary_top_k: usize,
    fallback_top_k: Option<usize>,
    repeat_window: usize,
    repeat_threshold: usize,
    triggered_queries: usize,
}

impl FallbackReport {
    fn from_options(options: &Options, measurements: &[QueryMeasurement]) -> Self {
        Self {
            strategy: match (
                options.weak_repeat_fallback_top_k.is_some(),
                options.weak_repeat_rerank,
            ) {
                (true, false) => "weak_repeat_top10_expand_k".to_string(),
                (false, true) => "weak_repeat_top10_rerank".to_string(),
                (false, false) => "none".to_string(),
                (true, true) => "invalid".to_string(),
            },
            primary_top_k: options.top_k,
            fallback_top_k: options.weak_repeat_fallback_top_k,
            repeat_window: WEAK_REPEAT_FALLBACK_WINDOW,
            repeat_threshold: WEAK_REPEAT_FALLBACK_THRESHOLD,
            triggered_queries: measurements
                .iter()
                .filter(|measurement| measurement.fallback_triggered)
                .count(),
        }
    }
}

fn metric_cutoff_label(fallback: &FallbackReport) -> String {
    fallback.fallback_top_k.map_or_else(
        || fallback.primary_top_k.to_string(),
        |fallback_top_k| format!("mixed{}-{fallback_top_k}", fallback.primary_top_k),
    )
}

#[derive(Debug, Clone, Serialize)]
struct RetrievedHit {
    uid: Uuid,
    score: f64,
    source_uri: Option<String>,
    title: Option<String>,
    lexical_backend: Option<String>,
    legs: Vec<String>,
}

impl RetrievedHit {
    fn from_hit(hit: &RetrievalHit) -> Self {
        let mut legs = Vec::new();
        if hit.legs.vector {
            legs.push("vector".to_string());
        }
        if hit.legs.lexical {
            legs.push("lexical".to_string());
        }
        if hit.legs.graph {
            legs.push("graph".to_string());
        }
        Self {
            uid: hit.uid,
            score: hit.score,
            source_uri: hit
                .knowledge_chunk
                .as_ref()
                .and_then(|chunk| chunk.source_uri.clone()),
            title: hit
                .knowledge_chunk
                .as_ref()
                .and_then(|chunk| chunk.source_title.clone()),
            lexical_backend: hit
                .lexical_backend
                .map(LexicalBackend::as_str)
                .map(str::to_string),
            legs,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct LatencyReport {
    query_embedding: LatencySummary,
    retrieval: LatencySummary,
    total: LatencySummary,
}

#[derive(Debug, Clone, Serialize, Default)]
struct LatencySummary {
    min_ms: u128,
    p50_ms: u128,
    p95_ms: u128,
    max_ms: u128,
    mean_ms: f64,
}

impl LatencySummary {
    fn from_values(mut values: Vec<u128>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        values.sort_unstable();
        let sum = values.iter().copied().sum::<u128>();
        let len = values.len();
        Self {
            min_ms: values[0],
            p50_ms: percentile(&values, 0.50),
            p95_ms: percentile(&values, 0.95),
            max_ms: values[len - 1],
            mean_ms: sum as f64 / len as f64,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CostReport {
    pricing_as_of: String,
    estimated_document_embed_tokens: u64,
    estimated_query_embed_tokens: u64,
    embedding_input_usd_per_million_tokens: f64,
    rerank_calls: u64,
    estimated_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ChunkingReport {
    target_tokens: usize,
    max_tokens: usize,
    min_tokens: usize,
}

impl From<ChunkingConfig> for ChunkingReport {
    fn from(config: ChunkingConfig) -> Self {
        Self {
            target_tokens: config.target_tokens,
            max_tokens: config.max_tokens,
            min_tokens: config.min_tokens,
        }
    }
}

fn percentile(values: &[u128], percentile: f64) -> u128 {
    let index = ((values.len().saturating_sub(1)) as f64 * percentile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_brain::retrieval::{KnowledgeChunkHydration, LegSources, SourceTier};
    use moa_memory_graph::NodeIndexRow;
    use serde_json::Value;

    use super::*;

    #[test]
    fn default_graph_policy_is_anchored_rescue() {
        // Pins: WixQA reports use the guarded graph policy unless a caller
        // explicitly opts into legacy broad expansion for A/B comparison.
        let options = Options::parse(std::iter::empty())
            .expect("default WixQA options should parse without args");

        assert_eq!(options.graph_policy, GraphRetrievalPolicy::AnchoredRescue);
        assert!(!options.disable_graph_expansion);
    }

    #[test]
    fn embedding_export_option_parses_output_path() {
        // Pins: the WixQA harness can write a reusable embedding bundle for
        // offline embedding experiments without changing the normal report path.
        let options = Options::parse(
            ["--embedding-export", ".moa/wixqa/exports/export.json"]
                .into_iter()
                .map(str::to_string),
        )
        .expect("embedding export option should parse");

        assert_eq!(
            options.embedding_export.as_deref(),
            Some(Path::new(".moa/wixqa/exports/export.json"))
        );
        assert_eq!(options.output, PathBuf::from(DEFAULT_OUTPUT));
    }

    #[test]
    fn top_graph_paths_reports_seed_and_path_identity_for_harmful_candidate() {
        // Pins: WixQA graph-harm diagnostics name the seed, seed source,
        // candidate, rank, article, hop, and path labels that produced harm.
        let seed = Uuid::from_u128(1);
        let harmful_candidate = Uuid::from_u128(2);
        let gold_candidate = Uuid::from_u128(3);
        let diagnostics = GraphRetrievalDiagnostics {
            path_traces: vec![
                GraphPathTrace {
                    seed_uid: seed,
                    seed_source: Some(GraphSeedSource::BroadFallback),
                    candidate_uid: harmful_candidate,
                    hop: 2,
                    edge_labels: vec!["MENTIONED_IN".to_string(), "RELATES_TO".to_string()],
                    edge_directions: vec!["incoming".to_string(), "outgoing".to_string()],
                },
                GraphPathTrace {
                    seed_uid: seed,
                    seed_source: Some(GraphSeedSource::BroadFallback),
                    candidate_uid: gold_candidate,
                    hop: 1,
                    edge_labels: vec!["RELATES_TO".to_string()],
                    edge_directions: vec!["outgoing".to_string()],
                },
            ],
            ..GraphRetrievalDiagnostics::new(GraphRetrievalPolicy::AnchoredRescue)
        };
        let url_to_article_id = HashMap::from([
            (
                "https://support.example.invalid/wrong".to_string(),
                "wrong".to_string(),
            ),
            (
                "https://support.example.invalid/gold".to_string(),
                "gold".to_string(),
            ),
        ]);
        let graph_hits = vec![
            graph_hit(harmful_candidate, "https://support.example.invalid/wrong"),
            graph_hit(gold_candidate, "https://support.example.invalid/gold"),
        ];

        let paths = top_graph_paths(
            &diagnostics,
            &graph_hits,
            &url_to_article_id,
            &["gold".to_string()],
        );

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].seed_uid, seed);
        assert_eq!(paths[0].seed_source, Some(GraphSeedSource::BroadFallback));
        assert_eq!(paths[0].candidate_uid, harmful_candidate);
        assert_eq!(paths[0].candidate_rank_with_graph, Some(1));
        assert_eq!(paths[0].candidate_article_id.as_deref(), Some("wrong"));
        assert_eq!(paths[0].hop, 2);
        assert_eq!(
            paths[0].edge_labels,
            vec!["MENTIONED_IN".to_string(), "RELATES_TO".to_string()]
        );
        assert_eq!(
            paths[0].edge_directions,
            vec!["incoming".to_string(), "outgoing".to_string()]
        );
    }

    #[test]
    fn graph_impact_classifier_distinguishes_hurt_rescue_and_neutral() {
        // Pins: graph-on/off comparisons classify rank movement by the first
        // relevant article, with missing relevance treated as worse than any rank.
        assert_eq!(
            classify_graph_impact(Some(3), Some(1), true),
            GraphImpact::Hurt
        );
        assert_eq!(
            classify_graph_impact(Some(1), Some(3), true),
            GraphImpact::Rescue
        );
        assert_eq!(
            classify_graph_impact(Some(2), Some(2), true),
            GraphImpact::Neutral
        );
        assert_eq!(
            classify_graph_impact(None, Some(4), true),
            GraphImpact::Hurt
        );
        assert_eq!(
            classify_graph_impact(Some(4), None, true),
            GraphImpact::Rescue
        );
        assert_eq!(
            classify_graph_impact(None, None, true),
            GraphImpact::Neutral
        );
        assert_eq!(
            classify_graph_impact(None, Some(4), false),
            GraphImpact::Neutral
        );
    }

    #[test]
    fn graph_report_aggregates_source_object_feature_contributions() {
        // Pins: WixQA reports expose SourceGraph feature totals and rank
        // movements instead of burying them in per-query retrieval diagnostics.
        let object_uid = Uuid::from_u128(42);
        let mut diagnostics = GraphRetrievalDiagnostics::new(GraphRetrievalPolicy::SourceGraph);
        diagnostics.source_object_ranking = moa_brain::retrieval::SourceObjectRankingDiagnostics {
            enabled: true,
            ranked_source_object_count: 2,
            feature_totals: SourceObjectFeatureContributions {
                max_fused_score: 1.0,
                lexical_title: 0.05,
                same_source_object_repeat: 0.02,
                exact_title_match: 0.04,
                typed_graph_evidence: 0.03,
                adjacent_chunk_support: 0.01,
                structural_only_penalty: -0.04,
            },
            top_source_objects: vec![SourceObjectFeatureContribution {
                object_uid,
                source_uri: Some("https://support.example.invalid/a".to_string()),
                source_title: Some("Custom domains".to_string()),
                chunk_count: 2,
                rank_before_source_graph: Some(3),
                rank_after_source_graph: 1,
                rank_delta_after_minus_before: Some(-2),
                score: 1.11,
                features: SourceObjectFeatureContributions {
                    max_fused_score: 1.0,
                    lexical_title: 0.05,
                    same_source_object_repeat: 0.02,
                    exact_title_match: 0.04,
                    typed_graph_evidence: 0.03,
                    adjacent_chunk_support: 0.01,
                    structural_only_penalty: -0.04,
                },
                typed_graph_evidence_count: 1,
                structural_only_graph_count: 1,
            }],
        };
        let measurements = vec![QueryMeasurement {
            question: "how do I connect a custom domain?".to_string(),
            gold_article_ids: vec!["a".to_string()],
            ranked_article_ids: vec!["a".to_string()],
            retrieved_hits: Vec::new(),
            graph_diagnostics: diagnostics,
            graph_comparison: None,
            metrics: QueryMetrics {
                hit: true,
                recall: 1.0,
                mrr: 1.0,
                ndcg: 1.0,
                first_relevant_rank: Some(1),
            },
            effective_top_k: 25,
            fallback_triggered: false,
            query_embedding_ms: 1,
            retrieval_ms: 2,
            total_ms: 3,
        }];

        let report = graph_diagnostics_report(
            &measurements,
            &Options {
                graph_policy: GraphRetrievalPolicy::SourceGraph,
                ..Options::parse(std::iter::empty()).expect("default options should parse")
            },
        );

        assert_eq!(report.source_object_ranking.enabled_query_count, 1);
        assert_eq!(report.source_object_ranking.ranked_source_object_count, 2);
        assert_eq!(
            report.source_object_ranking.feature_totals,
            SourceObjectFeatureContributions {
                max_fused_score: 1.0,
                lexical_title: 0.05,
                same_source_object_repeat: 0.02,
                exact_title_match: 0.04,
                typed_graph_evidence: 0.03,
                adjacent_chunk_support: 0.01,
                structural_only_penalty: -0.04,
            }
        );
        assert_eq!(report.source_object_ranking.top_rank_movements.len(), 1);
        assert_eq!(
            report.source_object_ranking.top_rank_movements[0].object_uid,
            object_uid
        );
    }

    fn graph_hit(uid: Uuid, source_uri: &str) -> RetrievalHit {
        RetrievalHit {
            uid,
            score: 1.0,
            legs: LegSources {
                graph: true,
                vector: false,
                lexical: false,
            },
            similarity: None,
            lexical_backend: None,
            source_tier: SourceTier::TenantKnowledge,
            knowledge_chunk: Some(KnowledgeChunkHydration {
                chunk_uid: Uuid::from_u128(10_000 + uid.as_u128()),
                document_version_uid: Uuid::from_u128(20_000 + uid.as_u128()),
                object_uid: Uuid::from_u128(30_000 + uid.as_u128()),
                chunk_hash: format!("hash-{uid}"),
                ordinal: 0,
                heading_path: vec!["Support".to_string()],
                text: "support article".to_string(),
                token_count: 8,
                metadata: Value::Null,
                source_uri: Some(source_uri.to_string()),
                source_title: Some("Support".to_string()),
                object_type: "article".to_string(),
                context_window: Vec::new(),
            }),
            node: NodeIndexRow {
                uid,
                label: NodeLabel::Fact,
                storage_partition_id: Some("tenant".to_string()),
                contact_id: None,
                scope: "tenant".to_string(),
                name: "support fact".to_string(),
                pii_class: PiiClass::None,
                valid_to: None,
                valid_from: Utc::now(),
                properties_summary: None,
                last_accessed_at: Utc::now(),
                quality_score: 0.5,
            },
        }
    }
}
