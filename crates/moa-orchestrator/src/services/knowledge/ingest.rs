//! Tenant knowledge ingestion runner and production dependency factories.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use moa_config::MoaConfig;
use moa_core::types::memory::{InformationBarrierId, RlsContext};
use moa_core::{traits::EmbeddingProvider, types::identifiers::TenantId};
use moa_crypto::KeyManagementProvider;
use moa_knowledge::{
    chunking::ChunkingConfig,
    domain::{KnowledgeSyncRun, ParseInput, ParsedDocument, ProviderAclCapability, RecordPage},
    ingestion::{
        KnowledgeIngestionPipeline, KnowledgeIngestionPipelineConfig, KnowledgeSourceAclContext,
        MemoryKnowledgeGraphWriter, PageIngestionReport,
    },
    parser::{
        DocumentParser, llamaparse::LlamaParseParser, native::NativeDocumentParser,
        reducto::ReductoParser, unstructured::UnstructuredParser,
    },
    providers::RecordContentFetcher,
    repository::PostgresKnowledgeRepository,
};
use moa_memory_graph::PostgresGraphStore;
use moa_memory_types::MemoryScope;
use moa_memory_vector::VectorStoreFactory;
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};

use super::KnowledgeServiceError;

/// Ingests provider record pages into tenant knowledge storage and graph/vector state.
#[async_trait]
pub trait KnowledgeIngestionRunner: Send + Sync {
    /// Applies one normalized provider record page for a stored sync run.
    async fn ingest_record_page(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        page: RecordPage,
    ) -> Result<PageIngestionReport, KnowledgeServiceError>;

    /// Tombstones active local objects absent from an exhaustive selected-source sync.
    async fn prune_unseen_objects(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport, KnowledgeServiceError>;
}

/// Production ingestion runner backed by Postgres, configured parsers, and graph memory stores.
#[derive(Clone)]
pub struct ProductionKnowledgeIngestionRunner {
    pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    config: MoaConfig,
    content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
}

impl ProductionKnowledgeIngestionRunner {
    /// Creates a production ingestion runner from the shared graph pool and runtime config.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, kms: Arc<dyn KeyManagementProvider>, config: MoaConfig) -> Self {
        Self {
            pool,
            kms,
            config,
            content_fetcher: None,
        }
    }

    /// Attaches a per-page content fetcher used to download byte content for
    /// records that carry neither inline text nor a directly fetchable URL.
    #[must_use]
    pub fn with_content_fetcher(
        mut self,
        content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
    ) -> Self {
        self.content_fetcher = content_fetcher;
        self
    }
}

#[async_trait]
impl KnowledgeIngestionRunner for ProductionKnowledgeIngestionRunner {
    async fn ingest_record_page(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        page: RecordPage,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        let parser_label = selected_parser_label(&self.config, run);
        let pipeline = build_ingestion_pipeline(
            self.pool.clone(),
            self.kms.clone(),
            run.tenant_id,
            &self.config,
            provider.to_string(),
            parser_label,
            run.information_barrier.clone(),
            self.content_fetcher.clone(),
        )
        .await?;
        pipeline
            .ingest_record_page(run.sync_run_uid, run.connection_uid, run.tenant_id, page)
            .await
            .map_err(KnowledgeServiceError::from)
    }

    async fn prune_unseen_objects(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        let parser_label = selected_parser_label(&self.config, run);
        // Pruning never materializes record content, so no content fetcher is
        // needed for this pipeline.
        let pipeline = build_ingestion_pipeline(
            self.pool.clone(),
            self.kms.clone(),
            run.tenant_id,
            &self.config,
            provider.to_string(),
            parser_label,
            run.information_barrier.clone(),
            None,
        )
        .await?;
        pipeline
            .prune_unseen_objects(
                run.sync_run_uid,
                run.connection_uid,
                run.tenant_id,
                seen_source_ids,
            )
            .await
            .map_err(KnowledgeServiceError::from)
    }
}

type ProductionKnowledgeIngestionPipeline = KnowledgeIngestionPipeline<
    PostgresKnowledgeRepository,
    ProductionDocumentParser,
    SharedEmbeddingProvider,
    MemoryKnowledgeGraphWriter<PostgresGraphStore>,
>;

#[allow(
    clippy::too_many_arguments,
    reason = "the production factory keeps storage, KMS, scope, parser, and content dependencies explicit"
)]
async fn build_ingestion_pipeline(
    pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    tenant_id: TenantId,
    config: &MoaConfig,
    provider: String,
    parser_label: String,
    information_barrier: Option<InformationBarrierId>,
    content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
) -> Result<ProductionKnowledgeIngestionPipeline, KnowledgeServiceError> {
    let scope = RlsContext::tenant(tenant_id);
    let graph_scope = scope
        .clone()
        .with_cleared_barriers(information_barrier.clone().into_iter().collect());
    let repository = Arc::new(PostgresKnowledgeRepository::scoped(
        pool.clone(),
        scope.clone(),
    ));
    // The connector's declared capability, not an operator preference, decides
    // whether this run's records are tenant-public or provider-managed. An
    // unrecognized provider is a typed error rather than a guess. No fingerprint
    // key is needed here: principals were keyed by the adapter during listing.
    let source_acl =
        KnowledgeSourceAclContext::for_capability(ProviderAclCapability::for_provider(&provider)?);
    let parser = Arc::new(build_document_parser(config, &parser_label)?);
    let embedder = Arc::new(SharedEmbeddingProvider::new(
        build_embedder_from_config(config, EmbedderConstructionRole::Ingestion)
            .map_err(embedder_config_error)?,
    ));
    let vector_backend = VectorStoreFactory::from_config(config).transactional_graph_backend(
        pool.clone(),
        graph_scope.clone(),
        false,
    );
    let graph_store = Arc::new(
        PostgresGraphStore::scoped(pool, graph_scope, kms)
            .with_vector_store(vector_backend.vector_store()),
    );
    let graph = Arc::new(MemoryKnowledgeGraphWriter::new(
        graph_store,
        MemoryScope::Tenant { tenant_id },
        "knowledge_sync_ingestion",
        information_barrier,
    ));
    Ok(KnowledgeIngestionPipeline::new(
        repository,
        parser,
        embedder,
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: chunking_config(config.knowledge.chunking),
            provider,
            parser_label,
        },
        source_acl,
    )
    .with_semantic_policy(config.knowledge.semantic)
    .with_content_fetcher(content_fetcher))
}

fn build_document_parser(
    config: &MoaConfig,
    parser_label: &str,
) -> Result<ProductionDocumentParser, KnowledgeServiceError> {
    match parser_label {
        "native" => {
            config.knowledge.selected_parser_api_key(parser_label)?;
            Ok(ProductionDocumentParser::Native(NativeDocumentParser::new()))
        }
        "llamaparse" => {
            let api_key = required_parser_api_key(
                parser_label,
                config.knowledge.selected_parser_api_key(parser_label)?,
            )?;
            Ok(ProductionDocumentParser::LlamaParse(
                LlamaParseParser::new(
                    config.knowledge.llamaparse.api_base_url.clone(),
                    api_key,
                    config.knowledge.llamaparse.tier.clone(),
                    config.knowledge.llamaparse.expand.clone(),
                )
                .map_err(KnowledgeServiceError::from)?,
            ))
        }
        "unstructured" => {
            let api_key = required_parser_api_key(
                parser_label,
                config.knowledge.selected_parser_api_key(parser_label)?,
            )?;
            Ok(ProductionDocumentParser::Unstructured(
                UnstructuredParser::new(
                    config.knowledge.unstructured.api_base_url.clone(),
                    api_key,
                    config.knowledge.unstructured.strategy.clone(),
                    config.knowledge.unstructured.chunking_strategy.clone(),
                )
                .map_err(KnowledgeServiceError::from)?,
            ))
        }
        "reducto" => {
            let api_key = required_parser_api_key(
                parser_label,
                config.knowledge.selected_parser_api_key(parser_label)?,
            )?;
            Ok(ProductionDocumentParser::Reducto(
                ReductoParser::new(
                    config.knowledge.reducto.api_base_url.clone(),
                    api_key,
                    config.knowledge.reducto.parse_mode.clone(),
                    config.knowledge.reducto.async_enabled,
                    config.knowledge.reducto.chunk_mode.clone(),
                    config.knowledge.reducto.force_url_result,
                )
                .map_err(KnowledgeServiceError::from)?,
            ))
        }
        other => Err(KnowledgeServiceError::InvalidRequest(format!(
            "knowledge parser `{other}` is not configured"
        ))),
    }
}

enum ProductionDocumentParser {
    Native(NativeDocumentParser),
    LlamaParse(LlamaParseParser),
    Unstructured(UnstructuredParser),
    Reducto(ReductoParser),
}

#[async_trait]
impl DocumentParser for ProductionDocumentParser {
    async fn parse(&self, input: ParseInput) -> moa_knowledge::Result<ParsedDocument> {
        match self {
            Self::Native(parser) => parser.parse(input).await,
            Self::LlamaParse(parser) => parser.parse(input).await,
            Self::Unstructured(parser) => parser.parse(input).await,
            Self::Reducto(parser) => parser.parse(input).await,
        }
    }
}

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

fn selected_parser_label(config: &MoaConfig, run: &KnowledgeSyncRun) -> String {
    run.parser
        .clone()
        .unwrap_or(config.knowledge.parser.external_default.clone())
}

fn chunking_config(config: moa_config::KnowledgeChunkingConfig) -> ChunkingConfig {
    ChunkingConfig {
        target_tokens: config.target_tokens,
        max_tokens: config.max_tokens,
        min_tokens: config.min_tokens,
    }
}

fn required_parser_api_key(
    parser_label: &str,
    api_key: Option<String>,
) -> Result<String, KnowledgeServiceError> {
    api_key.ok_or_else(|| {
        KnowledgeServiceError::InvalidRequest(format!(
            "knowledge parser `{parser_label}` requires an API key"
        ))
    })
}

fn embedder_config_error(error: moa_core::error::MoaError) -> KnowledgeServiceError {
    KnowledgeServiceError::Moa(moa_core::error::MoaError::ConfigError(error.to_string()))
}
