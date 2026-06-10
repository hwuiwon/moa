//! Retrieval perf-gate stack construction and workspace fixtures.

use super::*;

#[derive(Clone)]
pub(super) struct Stack {
    pub(super) database_url: String,
    pub(super) schema_name: String,
    pub(super) pool: PgPool,
    pub(super) embedder: Arc<CohereV4Embedder>,
    pub(super) workspaces: Vec<WorkspaceFixture>,
    pub(super) retrievers: Vec<Arc<WorkspaceRetriever>>,
}

impl Stack {
    pub(super) async fn up(database_url: &str, embedder: Arc<CohereV4Embedder>) -> Result<Self> {
        let schema_name = format!("perf_gate_{}", Uuid::now_v7().simple());
        let store = PostgresSessionStore::new_in_schema(database_url, &schema_name)
            .await
            .map_err(|error| anyhow!("failed to initialize perf schema: {error}"))?;
        let pool = store.pool().clone();
        drop(store);
        Ok(Self {
            database_url: database_url.to_string(),
            schema_name,
            pool,
            embedder,
            workspaces: Vec::new(),
            retrievers: Vec::new(),
        })
    }

    pub(super) async fn seed_workspaces(&mut self, cfg: &PerfGateConfig) -> Result<()> {
        let mut fixtures = Vec::with_capacity(cfg.workspaces);
        for workspace_index in 0..cfg.workspaces {
            let workspace_id = Uuid::now_v7();
            let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
            let vector = Arc::new(PgvectorStore::new_for_app_role(
                self.pool.clone(),
                scope.clone(),
            ));
            let graph = AgeGraphStore::scoped_for_app_role(self.pool.clone(), scope)
                .with_vector_store(vector);

            let fact_texts = (0..cfg.facts_per_workspace)
                .map(|fact_index| fact_text(workspace_index, fact_index))
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
                        uid,
                        label: NodeLabel::Fact,
                        workspace_id: Some(workspace_id.to_string()),
                        user_id: None,
                        scope: "workspace".to_string(),
                        name: text.clone(),
                        properties: json!({
                            "summary": text,
                            "workspace_index": workspace_index,
                            "fact_index": fact_index,
                            "source": "perf_gate",
                        }),
                        pii_class: PiiClass::None,
                        confidence: Some(0.9),
                        valid_from: Utc::now(),
                        embedding: Some(embedding),
                        embedding_model: Some(self.embedder.model_name().to_string()),
                        embedding_model_version: Some(self.embedder.model_version()),
                        actor_id: Uuid::now_v7().to_string(),
                        actor_kind: "system".to_string(),
                    })
                    .await
                    .map_err(|error| anyhow!("failed to seed graph node: {error}"))?;
            }
            seed_attack_dlq(&self.pool, workspace_id).await?;
            fixtures.push(WorkspaceFixture {
                workspace_id,
                first_uid: first_uid.context("workspace seeded no facts")?,
            });
        }
        self.workspaces = fixtures;
        Ok(())
    }

    pub(super) fn build_retrievers(&mut self) {
        self.retrievers = self
            .workspaces
            .iter()
            .map(|workspace| {
                Arc::new(WorkspaceRetriever::new(
                    self.pool.clone(),
                    workspace.workspace_id,
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
pub(super) struct WorkspaceFixture {
    pub(super) workspace_id: Uuid,
    pub(super) first_uid: Uuid,
}

pub(super) struct WorkspaceRetriever {
    scope: MemoryScope,
    cache: CachedHybridRetriever,
}

impl WorkspaceRetriever {
    fn new(pool: PgPool, workspace_id: Uuid) -> Self {
        let workspace = WorkspaceId::new(workspace_id.to_string());
        let scope_ctx = ScopeContext::workspace(workspace.clone());
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            pool.clone(),
            scope_ctx.clone(),
        ));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(pool.clone(), scope_ctx)
                .with_vector_store(vector.clone()),
        );
        let hybrid = HybridRetriever::new(pool.clone(), graph, vector).with_assume_app_role(true);
        Self {
            scope: MemoryScope::Workspace {
                workspace_id: workspace,
            },
            cache: CachedHybridRetriever::new_for_app_role(Arc::new(hybrid), pool),
        }
    }

    pub(super) async fn retrieve(&self, query: &RetrievalQuery) -> Result<usize> {
        let planned = PlannedQuery {
            strategy: Strategy::Both,
            seeds: Vec::new(),
            label_hint: Some(vec![NodeLabel::Fact]),
            scope: self.scope.clone(),
            scope_ancestors: self.scope.ancestors(),
            temporal_filter: None,
        };
        let request = RetrievalRequest {
            seeds: Vec::new(),
            query_text: query.text.clone(),
            query_embedding: query.embedding.clone(),
            scope: self.scope.clone(),
            label_filter: Some(vec![NodeLabel::Fact]),
            max_pii_class: PiiClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: Some(Strategy::Both),
            as_of: None,
        };
        let hits = self
            .cache
            .retrieve(&planned, request)
            .await
            .map_err(|error| anyhow!("retrieval failed: {error}"))?;
        Ok(hits.len())
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

pub(super) fn fact_text(workspace_index: usize, fact_index: usize) -> String {
    format!(
        "workspace {workspace_index} fact {fact_index} topic {} shard {} owner team{} retrieval memory record",
        fact_index % 17,
        fact_index % 31,
        workspace_index % 5
    )
}
