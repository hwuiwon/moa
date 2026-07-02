//! Stage 7: graph memory retrieval and prompt injection.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
#[cfg(test)]
use moa_core::AgentKnowledgePolicy;
use moa_core::RlsContext;
use moa_core::{
    AgentKnowledgeScopeMode, ContextMessage, ContextProcessor, ExcludedItem, LineageHandle,
    MoaError, NullLineageHandle, ProcessorOutput, QueryRewriteResult, Result, WorkingContext,
    traits::EmbeddingProvider,
};
use moa_memory_graph::{GraphStore, PiiClass, PostgresGraphStore};
use moa_memory_types::{MemoryScope, ScopeTier};
use moa_memory_vector::VectorStoreFactory;
use sqlx::PgPool;

use crate::retrieval::PlannedRetriever;

mod lineage;
mod rendering;
mod source_tiers;

use lineage::lineage_context_from_context;
use rendering::render_memory_context;
use source_tiers::{
    RetrievalScopePlan, admit_retrieval_hit, agent_knowledge_policy, dedupe_and_rank_hits,
    default_retrieval_plan, effective_max_pii_class, effective_result_limit,
};

const MEMORY_BUDGET_DIVISOR: usize = 5;
const GRAPH_MEMORY_RESULTS: usize = 4;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;
pub(crate) const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

struct ScopeRetrievalInput<'a> {
    ctx: &'a WorkingContext,
    query: &'a str,
    query_embedding: &'a [f32],
    scope_plan: &'a RetrievalScopePlan,
    emit_storage_lineage: bool,
    result_limit: usize,
    max_pii_class: PiiClass,
}

/// Injects graph-memory retrieval hits into the active turn context.
pub struct GraphMemoryRetriever {
    pool: PgPool,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    config: moa_core::MoaConfig,
    assume_app_role: bool,
    lineage: Arc<dyn LineageHandle>,
    result_limit: usize,
    planner: crate::planning::QueryPlanner,
    scoped_runtimes: Mutex<HashMap<MemoryScope, Arc<ScopedRetrievalRuntime>>>,
    runtime_factory: Arc<dyn ScopedRetrievalRuntimeFactory>,
}

/// Shared graph-memory retrieval stage backed by a process-wide retriever.
#[derive(Clone)]
pub struct SharedGraphMemoryRetriever {
    inner: Arc<GraphMemoryRetriever>,
}

impl SharedGraphMemoryRetriever {
    /// Creates a shared graph-memory processor from a process-wide retriever.
    #[must_use]
    pub fn new(inner: Arc<GraphMemoryRetriever>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ContextProcessor for SharedGraphMemoryRetriever {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn stage(&self) -> u8 {
        self.inner.stage()
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        self.inner.process(ctx).await
    }
}

/// Runtime backends for one graph-memory scope.
pub struct ScopedRetrievalRuntime {
    graph: Arc<dyn GraphStore>,
    hybrid: Arc<dyn PlannedRetriever>,
}

impl ScopedRetrievalRuntime {
    /// Creates a scoped runtime from graph planning and retrieval backends.
    #[must_use]
    pub fn new(graph: Arc<dyn GraphStore>, hybrid: Arc<dyn PlannedRetriever>) -> Self {
        Self { graph, hybrid }
    }
}

/// Factory for building graph-memory retrieval runtimes for individual scopes.
#[async_trait]
pub trait ScopedRetrievalRuntimeFactory: Send + Sync {
    /// Builds graph-memory backends for one memory scope.
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        config: &moa_core::MoaConfig,
        pool: &PgPool,
        assume_app_role: bool,
    ) -> Result<ScopedRetrievalRuntime>;
}

#[derive(Debug, Default)]
struct PostgresScopedRetrievalRuntimeFactory;

#[async_trait]
impl ScopedRetrievalRuntimeFactory for PostgresScopedRetrievalRuntimeFactory {
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        config: &moa_core::MoaConfig,
        pool: &PgPool,
        assume_app_role: bool,
    ) -> Result<ScopedRetrievalRuntime> {
        let scope_context = RlsContext::from(scope.clone());
        let vector_factory = VectorStoreFactory::from_config(config);
        let vector = vector_factory
            .configured_for_scope(pool, scope_context.clone(), assume_app_role)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!("vector backend selection failed: {error}"))
            })?;
        let graph_store = if assume_app_role {
            PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_context)
        } else {
            PostgresGraphStore::scoped(pool.clone(), scope_context)
        };
        let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
        let hybrid = Arc::new(
            crate::retrieval::HybridRetriever::from_config(
                config,
                pool.clone(),
                graph.clone(),
                vector,
            )
            .with_assume_app_role(assume_app_role),
        );
        let cached: Arc<dyn PlannedRetriever> = if assume_app_role {
            Arc::new(crate::retrieval::CachedHybridRetriever::new_for_app_role(
                hybrid,
                pool.clone(),
            ))
        } else {
            Arc::new(crate::retrieval::CachedHybridRetriever::new(
                hybrid,
                pool.clone(),
            ))
        };
        Ok(ScopedRetrievalRuntime::new(graph, cached))
    }
}

impl GraphMemoryRetriever {
    /// Creates a graph-memory retriever backed by the shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self::new_with_config(moa_core::MoaConfig::default(), pool, embedder)
    }

    /// Creates a graph-memory retriever backed by the shared Postgres pool and runtime config.
    #[must_use]
    pub fn new_with_config(
        config: moa_core::MoaConfig,
        pool: PgPool,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            pool,
            embedder,
            config,
            assume_app_role: false,
            lineage: Arc::new(NullLineageHandle),
            result_limit: GRAPH_MEMORY_RESULTS,
            planner: crate::planning::QueryPlanner::new(),
            scoped_runtimes: Mutex::new(HashMap::new()),
            runtime_factory: Arc::new(PostgresScopedRetrievalRuntimeFactory),
        }
    }

    /// Configures owner-role tests to assume the production app role during scoped reads.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Attaches the lineage sink used to capture retrieval traces.
    #[must_use]
    pub fn with_lineage(mut self, lineage: Arc<dyn LineageHandle>) -> Self {
        self.lineage = lineage;
        self
    }

    /// Overrides the number of final graph-memory hits injected into context.
    #[must_use]
    pub fn with_result_limit(mut self, result_limit: usize) -> Self {
        self.result_limit = result_limit;
        self
    }

    /// Overrides the scoped runtime factory used to build retrieval backends.
    #[must_use]
    pub fn with_scoped_runtime_factory(
        mut self,
        runtime_factory: Arc<dyn ScopedRetrievalRuntimeFactory>,
    ) -> Self {
        self.runtime_factory = runtime_factory;
        self.scoped_runtimes = Mutex::new(HashMap::new());
        self
    }

    /// Returns whether this retriever can run the vector leg.
    #[must_use]
    pub fn has_vector_retrieval(&self) -> bool {
        self.embedder.is_some()
    }

    async fn retrieve_hits(
        &self,
        ctx: &WorkingContext,
        query: String,
    ) -> Result<Vec<crate::retrieval::RetrievalHit>> {
        let policy = agent_knowledge_policy(ctx)?;
        let retrieval_plan = default_retrieval_plan(ctx, &policy);
        if retrieval_plan.is_empty() {
            return Ok(Vec::new());
        }
        let result_limit = effective_result_limit(&policy, self.result_limit);
        let max_pii_class = effective_max_pii_class(&policy)?;
        let query_embedding = if let Some(embedder) = self.embedder.as_deref() {
            embed_query(embedder, &query).await?
        } else {
            Vec::new()
        };
        let mut hits = Vec::new();
        for scope_plan in &retrieval_plan {
            let scope_hits = self
                .retrieve_hits_for_scope(ScopeRetrievalInput {
                    ctx,
                    query: &query,
                    query_embedding: &query_embedding,
                    scope_plan,
                    emit_storage_lineage: true,
                    result_limit,
                    max_pii_class,
                })
                .await?;
            hits.extend(
                scope_hits
                    .into_iter()
                    .filter_map(|hit| admit_retrieval_hit(hit, scope_plan, &policy)),
            );
        }
        Ok(dedupe_and_rank_hits(hits, result_limit))
    }

    async fn retrieve_hits_for_scope(
        &self,
        input: ScopeRetrievalInput<'_>,
    ) -> Result<Vec<crate::retrieval::RetrievalHit>> {
        let ScopeRetrievalInput {
            ctx,
            query,
            query_embedding,
            scope_plan,
            emit_storage_lineage,
            result_limit,
            max_pii_class,
        } = input;
        let scope = &scope_plan.scope;
        let runtime = self.runtime_for_scope(scope).await?;
        let planning = crate::planning::PlanningCtx::new(scope.clone(), runtime.graph.clone());
        let planned = self.planner.plan(query, &planning).await.map_err(|error| {
            moa_core::MoaError::StorageError(format!("graph memory planning failed: {error}"))
        })?;
        let mut request = planned.clone().into_retrieval_request(
            query.to_string(),
            query_embedding.to_vec(),
            max_pii_class,
            result_limit,
            true,
        );
        if let Some(label_filter) = &scope_plan.label_filter {
            request.label_filter = Some(label_filter.clone());
        }
        if emit_storage_lineage {
            request.lineage = Some(lineage_context_from_context(ctx));
        }
        runtime
            .hybrid
            .retrieve(&planned, request)
            .await
            .map_err(|error| {
                moa_core::MoaError::StorageError(format!("graph memory retrieval failed: {error}"))
            })
    }

    async fn runtime_for_scope(&self, scope: &MemoryScope) -> Result<Arc<ScopedRetrievalRuntime>> {
        if matches!(scope.tier(), ScopeTier::Contact) {
            return Ok(Arc::new(self.build_runtime_for_scope(scope).await?));
        }

        {
            let runtimes = self.scoped_runtimes.lock().map_err(|_| {
                moa_core::MoaError::StorageError(
                    "graph memory runtime cache lock poisoned".to_string(),
                )
            })?;
            if let Some(runtime) = runtimes.get(scope) {
                return Ok(runtime.clone());
            }
        }

        let runtime = Arc::new(self.build_runtime_for_scope(scope).await?);
        let mut runtimes = self.scoped_runtimes.lock().map_err(|_| {
            moa_core::MoaError::StorageError("graph memory runtime cache lock poisoned".to_string())
        })?;
        if let Some(existing) = runtimes.get(scope) {
            return Ok(existing.clone());
        }
        runtimes.insert(scope.clone(), runtime.clone());
        Ok(runtime)
    }

    async fn build_runtime_for_scope(&self, scope: &MemoryScope) -> Result<ScopedRetrievalRuntime> {
        self.runtime_factory
            .build_runtime(scope, &self.config, &self.pool, self.assume_app_role)
            .await
    }
}

#[async_trait]
impl ContextProcessor for GraphMemoryRetriever {
    fn name(&self) -> &str {
        "graph_memory"
    }

    fn stage(&self) -> u8 {
        7
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        if agent_knowledge_policy(ctx)?.mode == AgentKnowledgeScopeMode::Disabled {
            return Ok(ProcessorOutput {
                items_excluded: vec!["graph_memory".to_string()],
                excluded_items: vec![ExcludedItem {
                    item: "graph_memory".to_string(),
                    reason: "disabled by pinned agent knowledge policy".to_string(),
                }],
                ..ProcessorOutput::default()
            });
        }
        let Some(query) = extract_search_query(ctx) else {
            return Ok(ProcessorOutput::default());
        };
        let retrieval_started = Instant::now();
        let hits = self.retrieve_hits(ctx, query.clone()).await?;
        lineage::emit_retrieval_lineage(
            self.lineage.as_ref(),
            ctx,
            &query,
            &hits,
            retrieval_started.elapsed(),
        );
        if hits.is_empty() {
            return Ok(ProcessorOutput::default());
        }

        let tokens_before = ctx.token_count;
        let memory_budget = (ctx.token_budget / MEMORY_BUDGET_DIVISOR).max(MIN_PAGE_EXCERPT_TOKENS);
        let per_hit_budget = (memory_budget / hits.len().max(1)).max(MIN_PAGE_EXCERPT_TOKENS);
        let rendered = render_memory_context(&hits, per_hit_budget);

        let reminder = format!(
            "{MEMORY_REMINDER_PREFIX}\n{}\n</memory-reminder>",
            rendered.section
        );
        let insertion_index = trailing_user_insertion_index(&ctx.messages);
        ctx.insert_message(
            insertion_index,
            ContextMessage::user(reminder).with_source_refs(rendered.source_refs),
        );

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included: rendered.items_included,
            ..ProcessorOutput::default()
        })
    }
}

async fn embed_query(embedder: &dyn EmbeddingProvider, query: &str) -> Result<Vec<f32>> {
    let query_input = vec![query.to_string()];
    let embed_started = std::time::Instant::now();
    let mut embeddings = embedder.embed(&query_input).await?;
    metrics::histogram!("moa_retrieval_embedder_seconds")
        .record(embed_started.elapsed().as_secs_f64());
    embeddings.pop().ok_or_else(|| {
        MoaError::ProviderError("graph memory embedder returned no query embedding".to_string())
    })
}

#[cfg(test)]
fn retrieval_scopes_from_context(
    ctx: &WorkingContext,
    policy: &AgentKnowledgePolicy,
) -> Vec<RetrievalScopePlan> {
    default_retrieval_plan(ctx, policy)
}

fn trailing_user_insertion_index(messages: &[ContextMessage]) -> usize {
    let mut insertion_index = messages.len();
    while insertion_index > 0
        && matches!(
            messages[insertion_index - 1].role,
            moa_core::MessageRole::User
        )
    {
        insertion_index -= 1;
    }
    insertion_index
}

fn extract_search_query(ctx: &WorkingContext) -> Option<String> {
    if let Some(query) = ctx
        .metadata()
        .get("query_rewrite")
        .and_then(query_from_rewrite_metadata)
    {
        return Some(query);
    }

    extract_search_query_from_messages(&ctx.messages)
}

fn query_from_rewrite_metadata(value: &serde_json::Value) -> Option<String> {
    let result = serde_json::from_value::<QueryRewriteResult>(value.clone()).ok()?;
    let query = result.retrieval_query.trim();
    (!query.is_empty()).then(|| query.to_string())
}

fn extract_search_query_from_messages(messages: &[ContextMessage]) -> Option<String> {
    let text = messages
        .iter()
        .rev()
        .find_map(|message| match message.role {
            moa_core::MessageRole::User => Some(message.content.as_str()),
            _ => None,
        })?;
    let query = text.trim();
    (!query.is_empty()).then(|| query.to_string())
}

pub(crate) fn extract_search_keywords(text: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "about", "after", "again", "agent", "answer", "around", "because", "before", "being",
        "between", "could", "explain", "find", "from", "have", "into", "just", "like", "make",
        "need", "please", "respond", "should", "that", "the", "their", "them", "there", "these",
        "they", "this", "what", "when", "where", "which", "with", "would", "your",
    ];

    let mut keywords = Vec::new();
    for token in text
        .split(|character: char| {
            !(character.is_alphanumeric() || character == '_' || character == '-')
        })
        .map(str::trim)
        .filter(|token| token.len() >= 3)
    {
        let normalized = token.to_ascii_lowercase();
        if STOPWORDS.contains(&normalized.as_str()) || keywords.contains(&normalized) {
            continue;
        }
        keywords.push(normalized);
        if keywords.len() >= 6 {
            break;
        }
    }

    keywords
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        AgentContext, AgentKnowledgePolicy, AgentKnowledgeScopeMode, AgentPolicySnapshot, Channel,
        ContactId, ContactRef, ContactVerificationState, ContextProcessor, ModelCapabilities,
        ModelId, QueryRewriteResult, SessionId, SessionMeta, TenantId, TokenPricing,
        ToolCallFormat, WorkingContext,
    };
    use moa_lineage_core::TurnId;
    use moa_memory_graph::{
        EdgeLabel, EdgeWriteIntent, GraphExpansionHit, GraphStore, NodeIndexRow, NodeLabel,
        NodeWriteIntent, PiiClass,
    };
    use moa_memory_types::MemoryScope;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::{
        GraphMemoryRetriever, ScopedRetrievalRuntime, ScopedRetrievalRuntimeFactory,
        SharedGraphMemoryRetriever, extract_search_keywords, extract_search_query,
        retrieval_scopes_from_context,
    };

    #[derive(Debug)]
    struct NoopGraphStore;

    #[async_trait]
    impl GraphStore for NoopGraphStore {
        async fn create_node(&self, _intent: NodeWriteIntent) -> moa_memory_graph::Result<Uuid> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn supersede_node(
            &self,
            _old_uid: Uuid,
            _intent: NodeWriteIntent,
        ) -> moa_memory_graph::Result<Uuid> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn invalidate_node(&self, _uid: Uuid, _reason: &str) -> moa_memory_graph::Result<()> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn hard_purge(
            &self,
            _uid: Uuid,
            _redaction_marker: &str,
        ) -> moa_memory_graph::Result<()> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn create_edge(&self, _intent: EdgeWriteIntent) -> moa_memory_graph::Result<Uuid> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn get_node(&self, _uid: Uuid) -> moa_memory_graph::Result<Option<NodeIndexRow>> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn neighbors(
            &self,
            _seed: Uuid,
            _hops: u8,
            _edge_filter: Option<&[EdgeLabel]>,
            _as_of: Option<DateTime<Utc>>,
        ) -> moa_memory_graph::Result<Vec<NodeIndexRow>> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn expand_seeds(
            &self,
            _seeds: &[Uuid],
            _max_hops: u8,
            _as_of: Option<DateTime<Utc>>,
        ) -> moa_memory_graph::Result<Vec<GraphExpansionHit>> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn lookup_seeds(
            &self,
            _name: &str,
            _limit: i64,
            _as_of: Option<DateTime<Utc>>,
        ) -> moa_memory_graph::Result<Vec<NodeIndexRow>> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug)]
    struct NoopPlannedRetriever;

    #[async_trait]
    impl crate::retrieval::PlannedRetriever for NoopPlannedRetriever {
        async fn retrieve(
            &self,
            _planned: &crate::planning::PlannedQuery,
            _req: crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Vec<crate::retrieval::RetrievalHit>> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone, Debug)]
    struct CountingRuntimeFactory {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ScopedRetrievalRuntimeFactory for CountingRuntimeFactory {
        async fn build_runtime(
            &self,
            _scope: &MemoryScope,
            _config: &moa_core::MoaConfig,
            _pool: &sqlx::PgPool,
            _assume_app_role: bool,
        ) -> moa_core::Result<ScopedRetrievalRuntime> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ScopedRetrievalRuntime::new(
                Arc::new(NoopGraphStore),
                Arc::new(NoopPlannedRetriever),
            ))
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct RecordedRetrievalRequest {
        scope: MemoryScope,
        label_filter: Option<Vec<NodeLabel>>,
    }

    #[derive(Debug)]
    struct ScriptedPlannedRetriever {
        calls: Arc<Mutex<Vec<RecordedRetrievalRequest>>>,
        hits_by_scope: HashMap<MemoryScope, Vec<crate::retrieval::RetrievalHit>>,
    }

    #[async_trait]
    impl crate::retrieval::PlannedRetriever for ScriptedPlannedRetriever {
        async fn retrieve(
            &self,
            _planned: &crate::planning::PlannedQuery,
            req: crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Vec<crate::retrieval::RetrievalHit>> {
            self.calls
                .lock()
                .expect("scripted retriever calls lock")
                .push(RecordedRetrievalRequest {
                    scope: req.scope.clone(),
                    label_filter: req.label_filter.clone(),
                });
            Ok(self
                .hits_by_scope
                .get(&req.scope)
                .cloned()
                .unwrap_or_default())
        }
    }

    #[derive(Clone, Debug)]
    struct ScriptedRuntimeFactory {
        retriever: Arc<ScriptedPlannedRetriever>,
    }

    #[async_trait]
    impl ScopedRetrievalRuntimeFactory for ScriptedRuntimeFactory {
        async fn build_runtime(
            &self,
            _scope: &MemoryScope,
            _config: &moa_core::MoaConfig,
            _pool: &sqlx::PgPool,
            _assume_app_role: bool,
        ) -> moa_core::Result<ScopedRetrievalRuntime> {
            let retriever: Arc<dyn crate::retrieval::PlannedRetriever> = self.retriever.clone();
            Ok(ScopedRetrievalRuntime::new(
                Arc::new(NoopGraphStore),
                retriever,
            ))
        }
    }

    #[tokio::test]
    async fn shared_graph_memory_retriever_preserves_processor_identity() {
        // Pins: shared graph-memory runtime remains the stage-7 memory processor.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let shared = SharedGraphMemoryRetriever::new(std::sync::Arc::new(
            GraphMemoryRetriever::new(pool, None),
        ));

        assert_eq!(shared.name(), "graph_memory");
        assert_eq!(shared.stage(), 7);
    }

    #[tokio::test]
    async fn user_scoped_runtime_is_not_cached_in_process_lifetime_map() {
        // Pins: process-wide graph-memory retrievers must not retain one runtime per user.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let calls = Arc::new(AtomicUsize::new(0));
        let retriever = GraphMemoryRetriever::new(pool, None).with_scoped_runtime_factory(
            Arc::new(CountingRuntimeFactory {
                calls: calls.clone(),
            }),
        );
        let tenant_id = TenantId::new();
        let contact_scope = MemoryScope::Contact {
            tenant_id,
            contact_id: ContactId::new(),
        };
        let tenant_scope = MemoryScope::Tenant { tenant_id };

        retriever
            .runtime_for_scope(&contact_scope)
            .await
            .expect("contact runtime should build");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            retriever
                .scoped_runtimes
                .lock()
                .expect("runtime cache lock")
                .len(),
            0,
            "contact scopes should not grow the process-lifetime runtime cache"
        );

        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should build");
        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should be reused");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            retriever
                .scoped_runtimes
                .lock()
                .expect("runtime cache lock")
                .len(),
            1,
            "tenant scopes should still reuse one cached runtime"
        );
    }

    #[tokio::test]
    async fn scoped_runtime_factory_can_be_injected_without_postgres_stores() {
        // Pins: graph-memory tests can inject scope runtimes instead of constructing PG stores.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let calls = Arc::new(AtomicUsize::new(0));
        let retriever = GraphMemoryRetriever::new(pool, None).with_scoped_runtime_factory(
            Arc::new(CountingRuntimeFactory {
                calls: calls.clone(),
            }),
        );
        let tenant_id = TenantId::new();
        let tenant_scope = MemoryScope::Tenant { tenant_id };
        let contact_scope = MemoryScope::Contact {
            tenant_id,
            contact_id: ContactId::new(),
        };

        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should build from injected factory");
        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should be cached");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        retriever
            .runtime_for_scope(&contact_scope)
            .await
            .expect("contact runtime should build from injected factory");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn keyword_extraction_filters_stopwords_and_duplicates() {
        let keywords =
            extract_search_keywords("Please explain the OAuth refresh token race condition bug");

        assert_eq!(
            keywords,
            vec!["oauth", "refresh", "token", "race", "condition", "bug"]
        );
    }

    #[test]
    fn keyword_extraction_preserves_memory_article_ids() {
        let keywords = extract_search_keywords("What is news_article_001 about?");

        assert_eq!(keywords, vec!["news_article_001"]);
    }

    #[test]
    fn keyword_extraction_handles_empty_unicode_and_over_cap_input() {
        // Pins: empty input yields nothing; unicode word characters are retained
        // (only case-folded); and extraction caps at six distinct keywords.
        assert!(extract_search_keywords("").is_empty());

        assert_eq!(
            extract_search_keywords("café déjà OAuth"),
            vec!["café", "déjà", "oauth"]
        );

        let keywords = extract_search_keywords("alpha bravo charlie delta echo foxtrot golf hotel");
        assert_eq!(keywords.len(), 6);
        assert_eq!(
            keywords,
            vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
        );
    }

    #[test]
    fn original_rewrite_metadata_uses_full_query_for_retrieval() {
        // Pins: fail-open rewrites preserve the full semantic query instead of keyword-only fallback.
        let mut ctx = WorkingContext::new(
            &SessionMeta {
                id: SessionId::new(),
                tenant_id: TenantId::new(),
                channel: Channel::Chat,
                model: ModelId::new("mock"),
                ..SessionMeta::default()
            },
            capabilities(),
        );
        ctx.insert_metadata(
            "query_rewrite",
            serde_json::to_value(QueryRewriteResult::original(
                "Please explain the OAuth refresh token race condition bug",
            ))
            .expect("rewrite result should serialize"),
        );

        assert_eq!(
            extract_search_query(&ctx),
            Some("Please explain the OAuth refresh token race condition bug".to_string())
        );
    }

    #[test]
    fn original_rewrite_metadata_uses_latest_user_query_for_retrieval() {
        // Pins: skipped rewrite metadata preserves the full natural-language retrieval query.
        let mut ctx = WorkingContext::new(
            &SessionMeta {
                id: SessionId::new(),
                tenant_id: TenantId::new(),
                channel: Channel::Chat,
                model: ModelId::new("mock"),
                ..SessionMeta::default()
            },
            capabilities(),
        );
        ctx.append_message(moa_core::ContextMessage::user(
            "Please explain the OAuth refresh token race condition bug",
        ));

        assert_eq!(
            extract_search_query(&ctx),
            Some("Please explain the OAuth refresh token race condition bug".to_string())
        );
    }

    #[test]
    fn lineage_context_uses_compiled_turn_id_metadata() {
        // Pins: retrieval sidecar rows can join directly to turn-scoped lineage rows.
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        let turn_id = TurnId::new_v7();
        ctx.insert_metadata("_moa.turn_id", serde_json::json!(turn_id.0.to_string()));
        ctx.insert_metadata("_moa.turn_seq", serde_json::json!(42));

        let lineage = super::lineage_context_from_context(&ctx);

        assert_eq!(lineage.session_id, session.id);
        assert_eq!(lineage.turn_id, Some(turn_id));
        assert_eq!(lineage.turn_seq, 42);
    }

    #[tokio::test]
    async fn tenant_contact_knowledge_retrieval_disabled_policy_returns_no_memory() {
        // Pins: disabled configured-agent knowledge policy bypasses graph memory retrieval.
        let contact_id = ContactId::new();
        let mut session =
            contact_session(contact_id, ContactVerificationState::Verified, Vec::new());
        session.agent_context = Some(agent_context_with_knowledge_policy(AgentKnowledgePolicy {
            mode: AgentKnowledgeScopeMode::Disabled,
            ..AgentKnowledgePolicy::default()
        }));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), HashMap::new());
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::ContextMessage::user("Find relevant knowledge"));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("disabled memory retrieval should not fail");

        assert_eq!(
            calls.lock().expect("scripted retriever calls lock").len(),
            0,
            "disabled policy should not call scoped retrieval"
        );
        assert_eq!(ctx.messages.len(), 1, "disabled policy inserted memory");
        assert_eq!(output.items_included, Vec::<String>::new());
        assert_eq!(output.items_excluded, vec!["graph_memory".to_string()]);
    }

    #[test]
    fn tenant_contact_knowledge_retrieval_contact_context_plans_tenant_and_contact_scopes() {
        // Pins: contact sessions retrieve tenant knowledge plus only the current contact memory.
        let contact_id = ContactId::new();
        let session = contact_session(contact_id, ContactVerificationState::Verified, Vec::new());
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx, &AgentKnowledgePolicy::default());

        assert_eq!(
            plan,
            vec![
                super::RetrievalScopePlan {
                    scope: MemoryScope::Tenant {
                        tenant_id: session.tenant_id,
                    },
                    source_tier: crate::retrieval::SourceTier::TenantKnowledge,
                    label_filter: Some(vec![
                        NodeLabel::Document,
                        NodeLabel::Chunk,
                        NodeLabel::ContactGroup,
                    ]),
                },
                super::RetrievalScopePlan {
                    scope: MemoryScope::Contact {
                        tenant_id: session.tenant_id,
                        contact_id,
                    },
                    source_tier: crate::retrieval::SourceTier::UserMemory,
                    label_filter: None,
                },
            ]
        );
    }

    #[test]
    fn tenant_contact_knowledge_retrieval_no_contact_plans_tenant_only() {
        // Pins: sessions without an admitted contact retrieve tenant knowledge only.
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: None,
            ..SessionMeta::default()
        };
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx, &AgentKnowledgePolicy::default());

        assert_eq!(
            plan,
            vec![super::RetrievalScopePlan {
                scope: MemoryScope::Tenant {
                    tenant_id: session.tenant_id,
                },
                source_tier: crate::retrieval::SourceTier::TenantKnowledge,
                label_filter: Some(vec![
                    NodeLabel::Document,
                    NodeLabel::Chunk,
                    NodeLabel::ContactGroup,
                ]),
            }]
        );
    }

    #[test]
    fn tenant_contact_knowledge_retrieval_ignores_linked_cross_contact_scopes() {
        // Pins: verified contacts do not inherit linked-contact memory by default.
        let contact_id = ContactId::new();
        let linked_contact_id = ContactId::new();
        let session = contact_session(
            contact_id,
            ContactVerificationState::Verified,
            vec![contact_id, linked_contact_id, linked_contact_id],
        );
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx, &AgentKnowledgePolicy::default());

        assert_eq!(plan.len(), 2);
        assert!(plan.iter().any(|scope_plan| {
            scope_plan.scope
                == MemoryScope::Contact {
                    tenant_id: session.tenant_id,
                    contact_id,
                }
        }));
        assert!(!plan.iter().any(|scope_plan| {
            scope_plan.scope
                == MemoryScope::Contact {
                    tenant_id: session.tenant_id,
                    contact_id: linked_contact_id,
                }
        }));
    }

    #[tokio::test]
    async fn tenant_contact_knowledge_retrieval_context_separates_tenant_and_user_sections() {
        // Pins: prompt context keeps tenant knowledge and current-contact memory separate.
        let contact_id = ContactId::new();
        let linked_contact_id = ContactId::new();
        let session = contact_session(
            contact_id,
            ContactVerificationState::Verified,
            vec![linked_contact_id],
        );
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let contact_scope = MemoryScope::Contact {
            tenant_id: session.tenant_id,
            contact_id,
        };
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(
            tenant_scope.clone(),
            vec![
                retrieval_hit(
                    Uuid::from_u128(0x10),
                    session.tenant_id,
                    None,
                    NodeLabel::Fact,
                    "tenant",
                    crate::retrieval::SourceTier::TenantKnowledge,
                    "tenant operational fact",
                    "tenant fact should not be tenant knowledge",
                    0.99,
                ),
                retrieval_hit(
                    Uuid::from_u128(0x11),
                    session.tenant_id,
                    None,
                    NodeLabel::Chunk,
                    "tenant",
                    crate::retrieval::SourceTier::TenantKnowledge,
                    "tenant runbook chunk",
                    "tenant knowledge answer",
                    0.90,
                ),
            ],
        );
        hits_by_scope.insert(
            contact_scope,
            vec![
                retrieval_hit(
                    Uuid::from_u128(0x20),
                    session.tenant_id,
                    Some(contact_id),
                    NodeLabel::Fact,
                    "contact",
                    crate::retrieval::SourceTier::UserMemory,
                    "current contact preference",
                    "current user memory answer",
                    0.80,
                ),
                retrieval_hit(
                    Uuid::from_u128(0x21),
                    session.tenant_id,
                    Some(linked_contact_id),
                    NodeLabel::Fact,
                    "contact",
                    crate::retrieval::SourceTier::UserMemory,
                    "other contact preference",
                    "cross contact memory should not leak",
                    0.95,
                ),
            ],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), hits_by_scope);
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::ContextMessage::user(
            "Tell me the tenant and user memory answer",
        ));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("scripted retrieval should assemble context");

        let calls = calls.lock().expect("scripted retriever calls lock").clone();
        assert_eq!(
            calls,
            vec![
                RecordedRetrievalRequest {
                    scope: tenant_scope,
                    label_filter: Some(vec![
                        NodeLabel::Document,
                        NodeLabel::Chunk,
                        NodeLabel::ContactGroup,
                    ]),
                },
                RecordedRetrievalRequest {
                    scope: MemoryScope::Contact {
                        tenant_id: session.tenant_id,
                        contact_id,
                    },
                    label_filter: None,
                },
            ],
            "retriever should call tenant knowledge and current-contact scopes only"
        );
        assert_eq!(
            output.items_included,
            vec![
                "graph:Chunk:00000000-0000-0000-0000-000000000011".to_string(),
                "graph:Fact:00000000-0000-0000-0000-000000000020".to_string(),
            ]
        );
        let memory_message = ctx
            .messages
            .first()
            .expect("memory reminder should be inserted before user message");
        assert!(memory_message.content.contains("<knowledge_context>"));
        assert!(memory_message.content.contains("<tenant_knowledge>"));
        assert!(memory_message.content.contains("<user_memory>"));
        let tenant_section = section_between(
            &memory_message.content,
            "<tenant_knowledge>",
            "</tenant_knowledge>",
        );
        let user_section =
            section_between(&memory_message.content, "<user_memory>", "</user_memory>");
        assert!(tenant_section.contains("tenant knowledge answer"));
        assert!(!tenant_section.contains("current user memory answer"));
        assert!(user_section.contains("current user memory answer"));
        assert!(!user_section.contains("tenant knowledge answer"));
        assert!(
            !memory_message
                .content
                .contains("tenant fact should not be tenant knowledge")
        );
        assert!(
            !memory_message
                .content
                .contains("cross contact memory should not leak")
        );
    }

    fn scripted_graph_memory_retriever(
        calls: Arc<Mutex<Vec<RecordedRetrievalRequest>>>,
        hits_by_scope: HashMap<MemoryScope, Vec<crate::retrieval::RetrievalHit>>,
    ) -> GraphMemoryRetriever {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let retriever = Arc::new(ScriptedPlannedRetriever {
            calls,
            hits_by_scope,
        });
        GraphMemoryRetriever::new(pool, None)
            .with_scoped_runtime_factory(Arc::new(ScriptedRuntimeFactory { retriever }))
    }

    fn agent_context_with_knowledge_policy(policy: AgentKnowledgePolicy) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            knowledge_policy: policy,
            ..AgentPolicySnapshot::default()
        };
        let mut context = AgentContext::system_default();
        context.policy_snapshot =
            serde_json::to_value(snapshot).expect("policy snapshot should serialize");
        context
    }

    #[allow(clippy::too_many_arguments)]
    fn retrieval_hit(
        uid: Uuid,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        label: NodeLabel,
        scope: &str,
        source_tier: crate::retrieval::SourceTier,
        name: &str,
        summary: &str,
        score: f64,
    ) -> crate::retrieval::RetrievalHit {
        crate::retrieval::RetrievalHit {
            uid,
            score,
            legs: crate::retrieval::LegSources {
                graph: false,
                vector: false,
                lexical: true,
            },
            lexical_backend: Some(crate::retrieval::LexicalBackend::PostgresTsvector),
            source_tier,
            knowledge_chunk: None,
            node: NodeIndexRow {
                uid,
                label,
                storage_partition_id: Some(tenant_id.to_string()),
                contact_id: contact_id.map(|id| id.to_string()),
                scope: scope.to_string(),
                name: name.to_string(),
                pii_class: PiiClass::None,
                valid_to: None,
                valid_from: Utc::now(),
                properties_summary: Some(json!({ "summary": summary })),
                last_accessed_at: Utc::now(),
                quality_score: 0.5,
            },
        }
    }

    fn section_between<'a>(content: &'a str, start: &str, end: &str) -> &'a str {
        let start_index = content
            .find(start)
            .expect("section start marker should exist")
            + start.len();
        let end_index = content[start_index..]
            .find(end)
            .expect("section end marker should exist")
            + start_index;
        &content[start_index..end_index]
    }

    fn contact_session(
        contact_id: ContactId,
        state: ContactVerificationState,
        linked_contact_ids: Vec<ContactId>,
    ) -> SessionMeta {
        let tenant_id = TenantId::new();
        SessionMeta {
            id: SessionId::new(),
            tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: Some(ContactRef {
                contact_id,
                tenant_id,
                state,
                canonical_contact_id: None,
                linked_contact_ids,
                scopes: Vec::new(),
                permissions: serde_json::Value::Null,
                agent_ids: Vec::new(),
                session_ids: Vec::new(),
                verified_contact_point_ids: Vec::new(),
            }),
            ..SessionMeta::default()
        }
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("mock"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }
}
