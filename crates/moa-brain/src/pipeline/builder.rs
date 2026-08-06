//! Context pipeline assembly helpers.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::{
    traits::ContextProcessor, traits::EmbeddingProvider, traits::LLMProvider,
    traits::LineageHandle, traits::SegmentStore, traits::SessionStore,
};
use moa_crypto::KeyManagementProvider;
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};

use super::agent_instructions::AgentInstructionProcessor;
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

/// Inputs owned by the history stage.
pub struct HistoryStageInput {
    /// Optional LLM provider used by context compaction.
    pub compaction_llm_provider: Option<Arc<dyn LLMProvider>>,
}

/// Selects whether graph memory is shared or built for one pipeline.
pub enum GraphMemoryStageInput {
    /// Reuses a process-wide graph-memory retriever.
    Shared(Arc<GraphMemoryRetriever>),
    /// Builds a graph-memory retriever from these stage-local dependencies.
    Local {
        /// Postgres pool used by graph retrieval.
        graph_pool: sqlx::PgPool,
        /// KMS used to open sealed graph-memory content.
        kms: Arc<dyn KeyManagementProvider>,
        /// Optional retrieval embedder used by the local retriever.
        retrieval_embedder: Option<Arc<dyn EmbeddingProvider>>,
        /// Durable lineage handle used by retrieval.
        lineage: Arc<dyn LineageHandle>,
    },
}

/// Selects whether skill injection is shared or built for one pipeline.
pub enum SkillInjectionStageInput {
    /// Reuses a process-wide skill injector.
    Shared(Arc<SkillInjector>),
    /// Builds a skill injector from these stage-local dependencies.
    Local {
        /// Postgres pool used by the skill registry.
        graph_pool: sqlx::PgPool,
        /// Optional segment analytics store used by skill ranking.
        segment_store: Option<Arc<dyn SegmentStore>>,
        /// Optional embedder used by semantic skill ranking.
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    },
}

/// Inputs owned by the query-rewrite stage.
pub struct QueryRewriteStageInput {
    /// Optional LLM provider used by query rewriting.
    pub llm_provider: Option<Arc<dyn LLMProvider>>,
}

/// Inputs owned by runtime and tool-definition stages.
pub struct RuntimeStageInput {
    /// Optional identity prompt override for eval and harness runs.
    pub identity_prompt_override: Option<String>,
    /// Tool schemas to expose to the model.
    pub tool_schemas: Vec<serde_json::Value>,
}

/// Inputs owned by the optional standing-memory digest stage.
pub struct DigestStageInput {
    /// Postgres pool used to read standing memory digests.
    pub graph_pool: sqlx::PgPool,
}

/// Named stage inputs for graph-backed default context pipeline assembly.
pub struct GraphMemoryPipelineStages {
    /// History and compaction dependencies.
    pub history: HistoryStageInput,
    /// Graph retrieval mode and dependencies.
    pub graph_memory: GraphMemoryStageInput,
    /// Skill injection mode and dependencies.
    pub skill_injection: SkillInjectionStageInput,
    /// Query-rewrite dependencies.
    pub query_rewrite: QueryRewriteStageInput,
    /// Runtime reminder and tool-schema dependencies.
    pub runtime: RuntimeStageInput,
    /// Standing-memory digest dependencies.
    pub digest: DigestStageInput,
}

/// Builds the default graph-memory retriever and its retrieval embedder.
#[must_use]
pub fn build_default_graph_memory_retriever(
    config: &MoaConfig,
    graph_pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    lineage: Arc<dyn LineageHandle>,
) -> Arc<GraphMemoryRetriever> {
    let retrieval_embedder = match build_embedder_from_config(
        config,
        None,
        EmbedderConstructionRole::Retrieval,
    ) {
        Ok(embedder) => Some(embedder),
        Err(error) => {
            tracing::warn!(
                %error,
                "graph memory vector retrieval disabled because the retrieval embedder could not be constructed"
            );
            None
        }
    };

    build_graph_memory_retriever(config, graph_pool, kms, retrieval_embedder, lineage)
}

/// Builds a graph-memory retriever from caller-provided runtime dependencies.
#[must_use]
pub fn build_graph_memory_retriever(
    config: &MoaConfig,
    graph_pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    retrieval_embedder: Option<Arc<dyn EmbeddingProvider>>,
    lineage: Arc<dyn LineageHandle>,
) -> Arc<GraphMemoryRetriever> {
    Arc::new(
        GraphMemoryRetriever::new_with_config(config.clone(), graph_pool, kms, retrieval_embedder)
            .with_lineage(lineage),
    )
}

/// Builds the default context pipeline with graph-backed memory retrieval.
///
pub fn build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    stages: GraphMemoryPipelineStages,
) -> ContextPipeline {
    let GraphMemoryPipelineStages {
        history: HistoryStageInput {
            compaction_llm_provider,
        },
        graph_memory,
        skill_injection,
        query_rewrite: QueryRewriteStageInput { llm_provider },
        runtime:
            RuntimeStageInput {
                identity_prompt_override,
                tool_schemas,
            },
        digest: DigestStageInput {
            graph_pool: digest_pool,
        },
    } = stages;
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
    let graph_memory_retriever = match graph_memory {
        GraphMemoryStageInput::Shared(retriever) => retriever,
        GraphMemoryStageInput::Local {
            graph_pool,
            kms,
            retrieval_embedder,
            lineage,
        } => {
            if retrieval_embedder.is_some() {
                build_graph_memory_retriever(config, graph_pool, kms, retrieval_embedder, lineage)
            } else {
                build_default_graph_memory_retriever(config, graph_pool, kms, lineage)
            }
        }
    };
    let vector_retrieval_available = graph_memory_retriever.has_vector_retrieval();
    let query_rewriter: Option<Box<dyn ContextProcessor>> = if config.query_rewrite.enabled {
        llm_provider.map(|llm_provider| {
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
    // History compiles before the per-turn dynamic sections (skill manifest,
    // digest, retrieved memory) so those sections insert near the active user
    // turn instead of landing ahead of history, where their per-turn byte
    // churn would break provider prompt-cache reuse of the entire replayed
    // history and invalidate the incremental context snapshot every turn.
    stages.push(history);
    let skill_injector = match skill_injection {
        SkillInjectionStageInput::Shared(injector) => injector,
        SkillInjectionStageInput::Local {
            graph_pool,
            segment_store,
            embedder,
        } => {
            let injector = SkillInjector::new(graph_pool)
                .with_session_store(session_store.clone())
                .with_budget_config(config.skill_budget.clone());
            let injector = if let Some(segment_store) = segment_store {
                injector.with_segment_store(segment_store)
            } else {
                injector
            };
            let injector = if let Some(embedder) = embedder {
                injector.with_embedder(embedder)
            } else {
                injector
            };
            Arc::new(injector)
        }
    };
    stages.push(Box::new(SharedSkillInjector::new(skill_injector)));
    if config.memory.digest.enabled {
        stages.push(Box::new(DigestProcessor::new(
            digest_pool,
            config.memory.digest.clone(),
        )));
    }
    stages.extend([
        graph_memory,
        Box::new(RuntimeContextProcessor::default()) as Box<dyn ContextProcessor>,
    ]);

    ContextPipeline::with_runtime_limits(
        stages,
        config.budgets.daily_tenant_cents,
        config.context_snapshot.clone(),
    )
}
