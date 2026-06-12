//! Context pipeline assembly helpers.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    ContextProcessor, LLMProvider, LineageHandle, MoaConfig, SessionStore,
    record_context_pipeline_construction, record_retrieval_embedder_construction,
};
use moa_memory_vector::{EmbedderConstructionRole, build_embedder_from_config};

use super::compactor::Compactor;
use super::digest::DigestProcessor;
use super::history::HistoryCompiler;
use super::identity::IdentityProcessor;
use super::instructions::InstructionProcessor;
use super::memory::{GraphMemoryRetriever, SharedGraphMemoryRetriever};
use super::query_rewrite::QueryRewriter;
use super::runner::ContextPipeline;
use super::runtime_context::RuntimeContextProcessor;
use super::skills::SkillInjector;
use super::tools::ToolDefinitionProcessor;

/// Builds a context pipeline without a memory backend.
///
/// Production runtimes use
/// [`build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions`].
/// This helper remains useful for isolated pipeline and brain-loop tests that
/// do not provision graph-memory storage.
pub fn build_default_pipeline(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
) -> ContextPipeline {
    build_default_pipeline_with_tools(config, session_store, Vec::new())
}

/// Builds a context pipeline without memory and with a fixed tool loadout.
pub fn build_default_pipeline_with_tools(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    tool_schemas: Vec<serde_json::Value>,
) -> ContextPipeline {
    let history: Box<dyn ContextProcessor> = Box::new(
        HistoryCompiler::new(session_store.clone())
            .with_compaction_config(config.compaction.clone())
            .with_tool_output_config(config.tool_output.clone())
            .with_snapshot_config(config.context_snapshot.clone()),
    );
    let mut stages: Vec<Box<dyn ContextProcessor>> = vec![
        Box::new(IdentityProcessor::default()),
        Box::new(InstructionProcessor::new(
            config.general.workspace_instructions.clone(),
            config.general.user_instructions.clone(),
            None,
        )),
        Box::new(ToolDefinitionProcessor::new(tool_schemas)),
    ];
    stages.extend([
        history,
        Box::new(RuntimeContextProcessor::default()) as Box<dyn ContextProcessor>,
        Box::new(Compactor::new(
            config.compaction.clone(),
            session_store,
            None,
        )) as Box<dyn ContextProcessor>,
    ]);

    ContextPipeline::with_runtime_limits(
        stages,
        config.budgets.daily_workspace_cents,
        config.context_snapshot.clone(),
    )
}

/// Options for graph-backed default context pipeline assembly.
pub struct GraphMemoryPipelineOptions {
    /// Postgres pool used by graph retrieval.
    pub graph_pool: sqlx::PgPool,
    /// Optional process-wide graph-memory retriever reused across pipelines.
    pub shared_graph_memory_retriever: Option<Arc<GraphMemoryRetriever>>,
    /// Optional LLM provider used by context compaction.
    pub compaction_llm_provider: Option<Arc<dyn LLMProvider>>,
    /// Optional LLM provider used by query rewriting.
    pub query_rewrite_llm_provider: Option<Arc<dyn LLMProvider>>,
    /// Workspace instruction text discovered from the active repository.
    pub discovered_workspace_instructions: Option<String>,
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
        compaction_llm_provider,
        query_rewrite_llm_provider,
        discovered_workspace_instructions,
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
    let query_rewriter: Option<Box<dyn ContextProcessor>> = if config.query_rewrite.enabled {
        query_rewrite_llm_provider.map(|llm_provider| {
            Box::new(
                QueryRewriter::new(config.query_rewrite.clone(), llm_provider)
                    .with_session_store(session_store.clone()),
            ) as Box<dyn ContextProcessor>
        })
    } else {
        None
    };
    let graph_memory: Box<dyn ContextProcessor> = match shared_graph_memory_retriever {
        Some(retriever) => Box::new(SharedGraphMemoryRetriever::new(retriever)),
        None => Box::new(SharedGraphMemoryRetriever::new(
            build_default_graph_memory_retriever(config, graph_pool.clone(), lineage.clone()),
        )),
    };

    let mut stages: Vec<Box<dyn ContextProcessor>> = vec![
        Box::new(IdentityProcessor::default()),
        Box::new(InstructionProcessor::new(
            config.general.workspace_instructions.clone(),
            config.general.user_instructions.clone(),
            discovered_workspace_instructions,
        )),
        Box::new(ToolDefinitionProcessor::new(tool_schemas)),
    ];
    if let Some(query_rewriter) = query_rewriter {
        stages.push(query_rewriter);
    }
    stages.push(Box::new(
        SkillInjector::new(graph_pool.clone())
            .with_session_store(session_store.clone())
            .with_budget_config(config.skill_budget.clone()),
    ));
    if config.memory.digest.enabled {
        stages.push(Box::new(DigestProcessor::new(
            graph_pool.clone(),
            config.memory.digest.clone(),
        )));
    }
    stages.extend([
        graph_memory,
        history,
        Box::new(RuntimeContextProcessor::default()) as Box<dyn ContextProcessor>,
        Box::new(Compactor::new(
            config.compaction.clone(),
            session_store,
            compaction_llm_provider,
        )) as Box<dyn ContextProcessor>,
    ]);

    let pipeline = ContextPipeline::with_runtime_limits(
        stages,
        config.budgets.daily_workspace_cents,
        config.context_snapshot.clone(),
    );
    record_context_pipeline_construction(pipeline_started.elapsed());
    pipeline
}
