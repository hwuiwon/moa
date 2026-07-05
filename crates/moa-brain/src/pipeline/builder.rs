//! Context pipeline assembly helpers.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    ContextProcessor, LLMProvider, LineageHandle, MoaConfig, SegmentStore, SessionStore,
    traits::EmbeddingProvider,
};
use moa_observability::{
    record_context_pipeline_construction, record_retrieval_embedder_construction,
};
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};

use super::agent_instructions::AgentInstructionProcessor;
use super::delegation_planning::DelegationPlanningProcessor;
use super::digest::DigestProcessor;
use super::history::HistoryCompiler;
use super::identity::{DEFAULT_IDENTITY_PROMPT, IdentityProcessor};
use super::instructions::InstructionProcessor;
use super::memory::{GraphMemoryRetriever, SharedGraphMemoryRetriever};
use super::query_rewrite::QueryRewriter;
use super::runner::ContextPipeline;
use super::runtime_context::RuntimeContextProcessor;
use super::skills::{SharedSkillInjector, SkillInjector};
use super::tools::ToolDefinitionProcessor;

/// Options for graph-backed default context pipeline assembly.
pub struct GraphMemoryPipelineOptions {
    /// Postgres pool used by graph retrieval.
    pub graph_pool: sqlx::PgPool,
    /// Optional process-wide graph-memory retriever reused across pipelines.
    pub shared_graph_memory_retriever: Option<Arc<GraphMemoryRetriever>>,
    /// Optional retrieval embedder used when building a graph-memory retriever locally.
    pub retrieval_embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Optional process-wide skill injector reused across pipelines.
    pub shared_skill_injector: Option<Arc<SkillInjector>>,
    /// Optional segment analytics store used by skill-ranking signals.
    pub segment_store: Option<Arc<dyn SegmentStore>>,
    /// Optional LLM provider used by context compaction.
    pub compaction_llm_provider: Option<Arc<dyn LLMProvider>>,
    /// Optional LLM provider used by query rewriting.
    pub query_rewrite_llm_provider: Option<Arc<dyn LLMProvider>>,
    /// Optional identity prompt override for eval and harness runs.
    pub identity_prompt_override: Option<String>,
    /// Tool schemas to expose to the model.
    pub tool_schemas: Vec<serde_json::Value>,
    /// Durable lineage handle used by retrieval stages.
    pub lineage: Arc<dyn LineageHandle>,
}

/// Builds the default graph-memory retriever and its retrieval embedder.
#[must_use]
pub fn build_default_graph_memory_retriever(
    config: &MoaConfig,
    graph_pool: sqlx::PgPool,
    lineage: Arc<dyn LineageHandle>,
) -> Arc<GraphMemoryRetriever> {
    let embedder_started = Instant::now();
    let retrieval_embedder = match build_embedder_from_config(
        config,
        EmbedderConstructionRole::Retrieval,
    ) {
        Ok(embedder) => {
            record_retrieval_embedder_construction("success", embedder_started.elapsed());
            Some(embedder)
        }
        Err(error) => {
            record_retrieval_embedder_construction("failure", embedder_started.elapsed());
            tracing::warn!(
                %error,
                "graph memory vector retrieval disabled because the retrieval embedder could not be constructed"
            );
            None
        }
    };

    build_graph_memory_retriever(config, graph_pool, retrieval_embedder, lineage)
}

/// Builds a graph-memory retriever from caller-provided runtime dependencies.
#[must_use]
pub fn build_graph_memory_retriever(
    config: &MoaConfig,
    graph_pool: sqlx::PgPool,
    retrieval_embedder: Option<Arc<dyn EmbeddingProvider>>,
    lineage: Arc<dyn LineageHandle>,
) -> Arc<GraphMemoryRetriever> {
    Arc::new(
        GraphMemoryRetriever::new_with_config(config.clone(), graph_pool, retrieval_embedder)
            .with_lineage(lineage),
    )
}

/// Builds the default context pipeline with graph-backed memory retrieval.
///
pub fn build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    options: GraphMemoryPipelineOptions,
) -> ContextPipeline {
    let pipeline_started = Instant::now();
    let GraphMemoryPipelineOptions {
        graph_pool,
        shared_graph_memory_retriever,
        retrieval_embedder,
        shared_skill_injector,
        segment_store,
        compaction_llm_provider,
        query_rewrite_llm_provider,
        identity_prompt_override,
        tool_schemas,
        lineage,
    } = options;
    let history: Box<dyn ContextProcessor> =
        if let Some(llm_provider) = compaction_llm_provider.clone() {
            Box::new(
                HistoryCompiler::with_compaction(
                    session_store.clone(),
                    llm_provider,
                    config.compaction.clone(),
                )
                .with_tool_output_config(config.tool_output.clone())
                .with_snapshot_config(config.context_snapshot.clone()),
            )
        } else {
            Box::new(
                HistoryCompiler::new(session_store.clone())
                    .with_compaction_config(config.compaction.clone())
                    .with_tool_output_config(config.tool_output.clone())
                    .with_snapshot_config(config.context_snapshot.clone()),
            )
        };
    let graph_memory_retriever = shared_graph_memory_retriever.unwrap_or_else(|| {
        if retrieval_embedder.is_some() {
            build_graph_memory_retriever(
                config,
                graph_pool.clone(),
                retrieval_embedder,
                lineage.clone(),
            )
        } else {
            build_default_graph_memory_retriever(config, graph_pool.clone(), lineage.clone())
        }
    });
    let vector_retrieval_available = graph_memory_retriever.has_vector_retrieval();
    let query_rewriter: Option<Box<dyn ContextProcessor>> = if config.query_rewrite.enabled {
        query_rewrite_llm_provider.map(|llm_provider| {
            Box::new(
                QueryRewriter::new_with_shared_circuit(
                    config.query_rewrite.clone(),
                    llm_provider,
                    "default_pipeline",
                )
                .with_session_store(session_store.clone())
                .with_retrieval_availability(true, vector_retrieval_available),
            ) as Box<dyn ContextProcessor>
        })
    } else {
        None
    };
    let graph_memory: Box<dyn ContextProcessor> =
        Box::new(SharedGraphMemoryRetriever::new(graph_memory_retriever));
    let identity_prompt =
        identity_prompt_override.unwrap_or_else(|| DEFAULT_IDENTITY_PROMPT.to_string());

    let mut stages: Vec<Box<dyn ContextProcessor>> = vec![
        Box::new(IdentityProcessor::new(identity_prompt)),
        Box::new(AgentInstructionProcessor::new()),
        Box::new(InstructionProcessor::new(
            config.general.workspace_instructions.clone(),
            config.general.user_instructions.clone(),
        )),
        Box::new(ToolDefinitionProcessor::new(tool_schemas)),
    ];
    if let Some(query_rewriter) = query_rewriter {
        stages.push(query_rewriter);
    }
    let skill_injector = shared_skill_injector.unwrap_or_else(|| {
        let injector = SkillInjector::new(graph_pool.clone())
            .with_session_store(session_store.clone())
            .with_budget_config(config.skill_budget.clone());
        let injector = if let Some(segment_store) = segment_store {
            injector.with_segment_store(segment_store)
        } else {
            injector
        };
        Arc::new(injector)
    });
    stages.push(Box::new(SharedSkillInjector::new(skill_injector)));
    if config.memory.digest.enabled {
        stages.push(Box::new(DigestProcessor::new(
            graph_pool.clone(),
            config.memory.digest.clone(),
        )));
    }
    stages.extend([
        graph_memory,
        history,
        Box::new(DelegationPlanningProcessor::new()) as Box<dyn ContextProcessor>,
        Box::new(RuntimeContextProcessor::default()) as Box<dyn ContextProcessor>,
    ]);

    let pipeline = ContextPipeline::with_runtime_limits(
        stages,
        config.budgets.daily_tenant_cents,
        config.context_snapshot.clone(),
    );
    record_context_pipeline_construction(pipeline_started.elapsed());
    pipeline
}
