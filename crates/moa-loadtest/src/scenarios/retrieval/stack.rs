//! Retrieval perf-gate stack construction and tenant fixtures.

use super::*;

#[derive(Clone)]
pub(super) struct Stack {
    pub(super) database_url: String,
    pub(super) schema_name: String,
    pub(super) pool: PgPool,
    pub(super) kms: Arc<dyn KeyManagementProvider>,
    pub(super) embedder: Arc<dyn EmbeddingProvider>,
    pub(super) tenants: Vec<TenantFixture>,
    pub(super) retrievers: Vec<Arc<TenantRetriever>>,
}

impl Stack {
    pub(super) async fn up(
        maintenance_url: &str,
        embedder: Arc<dyn EmbeddingProvider>,
    ) -> Result<Self> {
        let (database_url, schema_name) = provision_cloned_database_from(maintenance_url)
            .await
            .map_err(|error| anyhow!("failed to provision perf database: {error}"))?;
        let store = match PostgresSessionStore::new_in_existing_schema(&database_url, &schema_name)
            .await
        {
            Ok(store) => store,
            Err(error) => {
                if let Err(cleanup_error) = cleanup_test_schema(&database_url, &schema_name).await {
                    tracing::warn!(
                        %cleanup_error,
                        "failed to clean up perf clone after store initialization failed"
                    );
                }
                return Err(anyhow!("failed to initialize perf database: {error}"));
            }
        };
        let pool = store.pool().clone();
        drop(store);
        Ok(Self {
            database_url,
            schema_name,
            pool,
            kms: Arc::new(LocalKmsProvider::new()),
            embedder,
            tenants: Vec::new(),
            retrievers: Vec::new(),
        })
    }

    pub(super) async fn seed_tenants(&mut self, cfg: &PerfGateConfig) -> Result<()> {
        let mut fixtures = Vec::with_capacity(cfg.tenants);
        for tenant_index in 0..cfg.tenants {
            let tenant_id = TenantId::new();
            let scope = RlsContext::tenant(tenant_id);
            let vector = Arc::new(PgvectorStore::new_for_app_role(
                self.pool.clone(),
                scope.clone(),
            ));
            let graph =
                PostgresGraphStore::scoped_for_app_role(self.pool.clone(), scope, self.kms.clone())
                    .with_vector_store(vector);

            let fact_texts = (0..cfg.facts_per_tenant)
                .map(|fact_index| fact_text(tenant_index, fact_index))
                .collect::<Vec<_>>();
            let embeddings = embed_texts(self.embedder.as_ref(), &fact_texts).await?;
            let mut first_uid = None;
            for (fact_index, (text, embedding)) in
                fact_texts.into_iter().zip(embeddings).enumerate()
            {
                let uid = Uuid::now_v7();
                if first_uid.is_none() {
                    first_uid = Some(uid);
                }
                graph
                    .create_node(NodeWriteIntent {
                        barrier: None,
                        uid,
                        data_subject_id: tenant_id.0,
                        label: NodeLabel::Fact,
                        storage_partition_id: Some(
                            StoragePartitionId::for_tenant(tenant_id).to_string(),
                        ),
                        contact_id: None,
                        scope: "tenant".to_string(),
                        name: text.clone(),
                        properties: json!({
                            "summary": text,
                            "tenant_index": tenant_index,
                            "fact_index": fact_index,
                            "source": "perf_gate",
                        }),
                        pii_class: SensitivityClass::None,
                        confidence: Some(0.9),
                        valid_from: Utc::now(),
                        embedding: Some(embedding),
                        embedding_model: Some(self.embedder.model_id().to_string()),
                        embedding_model_version: Some(self.embedder.model_version()),
                        embedding_text: None,
                        actor_id: Uuid::now_v7().to_string(),
                        actor_kind: "system".to_string(),
                    })
                    .await
                    .map_err(|error| anyhow!("failed to seed graph node: {error}"))?;
            }
            seed_attack_dlq(&self.pool, tenant_id).await?;
            fixtures.push(TenantFixture {
                tenant_id,
                first_uid: first_uid.context("tenant seeded no facts")?,
            });
        }
        self.tenants = fixtures;
        Ok(())
    }

    pub(super) fn build_retrievers(&mut self) {
        self.retrievers = self
            .tenants
            .iter()
            .map(|tenant| {
                Arc::new(TenantRetriever::new(
                    self.pool.clone(),
                    self.kms.clone(),
                    tenant.tenant_id,
                ))
            })
            .collect();
    }

    pub(super) async fn cleanup(self) -> Result<()> {
        self.pool.close().await;
        cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(|error| anyhow!("failed to cleanup perf schema: {error}"))
    }
}

#[derive(Debug, Clone)]
pub(super) struct TenantFixture {
    pub(super) tenant_id: TenantId,
    pub(super) first_uid: Uuid,
}

pub(super) struct TenantRetriever {
    scope: MemoryScope,
    cache: CachedHybridRetriever,
}

impl TenantRetriever {
    fn new(pool: PgPool, kms: Arc<dyn KeyManagementProvider>, tenant_id: TenantId) -> Self {
        let scope_ctx = RlsContext::tenant(tenant_id);
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            pool.clone(),
            scope_ctx.clone(),
        ));
        let graph = Arc::new(
            PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_ctx, kms)
                .with_vector_store(vector.clone()),
        );
        let hybrid = HybridRetriever::new(pool.clone(), graph, vector).with_assume_app_role(true);
        Self {
            scope: MemoryScope::Tenant { tenant_id },
            cache: CachedHybridRetriever::new_for_app_role(Arc::new(hybrid), pool),
        }
    }

    pub(super) async fn retrieve(&self, query: &RetrievalQuery) -> Result<usize> {
        let planned = PlannedQuery {
            strategy: Strategy::Both,
            seeds: Vec::new(),
            label_hint: Some(vec![NodeLabel::Fact]),
            scope: self.scope.clone(),
            temporal_filter: None,
        };
        let request = RetrievalRequest {
            source_acl: moa_core::types::memory::SourceAclContext::empty(0),
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: query.text.clone(),
            query_embedding: Some(query.embedding.clone()),
            scope: self.scope.clone(),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: Some(Strategy::Both),
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        };
        let output = self
            .cache
            .retrieve(&planned, request)
            .await
            .map_err(|error| anyhow!("retrieval failed: {error}"))?;
        Ok(output.hits.len())
    }
}

pub(super) async fn embed_texts(
    embedder: &dyn EmbeddingProvider,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let started = Instant::now();
    let embeddings = embedder
        .embed(texts)
        .await
        .map_err(|error| anyhow!("embedding provider failed: {error}"))?;
    let per_text = if texts.is_empty() {
        0.0
    } else {
        started.elapsed().as_secs_f64() / texts.len() as f64
    };
    for _ in texts {
        metrics::histogram!("moa_retrieval_embedder_seconds").record(per_text);
    }
    Ok(embeddings)
}

pub(super) fn fact_text(tenant_index: usize, fact_index: usize) -> String {
    format!(
        "tenant {tenant_index} fact {fact_index} topic {} shard {} owner team{} retrieval memory record",
        fact_index % 17,
        fact_index % 31,
        tenant_index % 5
    )
}
