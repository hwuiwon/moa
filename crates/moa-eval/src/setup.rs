//! Isolated environment construction for eval runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use moa_brain::{
    ContextPipeline, GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    pipeline::identity::DEFAULT_IDENTITY_PROMPT,
};
use moa_core::{
    ActionPolicyEffect, ExperienceStore, LLMProvider, LearningCandidateStore, LineageHandle,
    MoaConfig, SegmentStore, SessionActorRef, SessionMeta, SessionStore, StoragePartitionId,
    TenantId, UserId,
};
use moa_eval_core::{ActionPolicyOverride, AgentConfig, EvalError, InstructionOverride, Result};
use moa_hands::ToolRouter;
use moa_providers::ProviderRegistry;
use moa_security::{ActionPolicies, ActionPolicyRuleStore};
use moa_session::PostgresSessionStore;
use serde_json::Value;
use tokio::fs;
use uuid::Uuid;

use crate::memory_eval::tenant_id_from_label;

const DEFAULT_EVAL_USER: &str = "eval-runner";

/// Fully isolated runtime environment for one eval execution.
pub struct AgentEnvironment {
    /// Session store scoped to this run.
    pub session_store: Arc<dyn SessionStore>,
    /// Segment persistence and analytics scoped to this run.
    pub segment_store: Arc<dyn SegmentStore>,
    /// Experience persistence scoped to this run.
    pub experience_store: Arc<dyn ExperienceStore>,
    /// Learning candidate persistence scoped to this run.
    pub learning_candidate_store: Arc<dyn LearningCandidateStore>,
    /// LLM provider used for the run.
    pub llm_provider: Arc<dyn LLMProvider>,
    /// Tool router with per-config restrictions and policies applied.
    pub tool_router: Arc<ToolRouter>,
    /// Context pipeline used to compile requests.
    pub pipeline: ContextPipeline,
    /// Temporary workspace directory used as the sandbox root.
    pub workspace_dir: PathBuf,
    /// Persisted session identifier for the run.
    pub session_id: moa_core::SessionId,
    /// Storage partition identifier used inside the run.
    pub storage_partition_id: StoragePartitionId,
    /// User identifier used inside the run.
    pub user_id: UserId,
    /// In-memory lineage handle used to retain eval-run lineage events.
    pub lineage: Arc<EvalLineageHandle>,
}

/// In-memory lineage handle for isolated eval runs.
#[derive(Debug, Default)]
pub struct EvalLineageHandle {
    events: Mutex<Vec<Value>>,
}

impl EvalLineageHandle {
    /// Returns a snapshot of all captured lineage JSON events.
    #[must_use]
    pub fn events(&self) -> Vec<Value> {
        match self.events.lock() {
            Ok(events) => events.clone(),
            Err(error) => {
                tracing::warn!(%error, "failed to read eval lineage events");
                Vec::new()
            }
        }
    }
}

impl LineageHandle for EvalLineageHandle {
    fn record(&self, evt_json: Value) {
        match self.events.lock() {
            Ok(mut events) => events.push(evt_json),
            Err(error) => tracing::warn!(%error, "failed to record eval lineage event"),
        }
    }
}

/// Builds a complete isolated agent environment from an agent config.
pub async fn build_agent_environment(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    temp_dir: &Path,
) -> Result<AgentEnvironment> {
    let provider_registry = ProviderRegistry::from_config(base_config);
    let requested_model = agent_config
        .model
        .as_deref()
        .unwrap_or(base_config.models.main.as_str());
    let llm_provider = provider_registry.provider_for_model(Some(requested_model))?;
    build_agent_environment_with_provider(base_config, agent_config, temp_dir, llm_provider).await
}

/// Builds an isolated agent environment using an explicit provider instance.
pub(crate) async fn build_agent_environment_with_provider(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    temp_dir: &Path,
    llm_provider: Arc<dyn LLMProvider>,
) -> Result<AgentEnvironment> {
    let run_root = temp_dir.join(format!("eval-{}", Uuid::now_v7()));
    let workspace_dir = run_root.join("workspace");
    fs::create_dir_all(&workspace_dir)
        .await
        .map_err(|source| EvalError::Io {
            path: workspace_dir.clone(),
            source,
        })?;

    let tenant_id = eval_tenant_id_for_agent(&agent_config.name);
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let user_id = UserId::new(DEFAULT_EVAL_USER);
    let lineage = Arc::new(EvalLineageHandle::default());
    let schema_name = format!("eval_{}", Uuid::now_v7().simple());
    let session_store_concrete = Arc::new(
        PostgresSessionStore::new_in_schema(&base_config.database.url, &schema_name).await?,
    );
    let session_store: Arc<dyn SessionStore> = session_store_concrete.clone();
    let segment_store: Arc<dyn SegmentStore> = session_store_concrete.clone();
    let experience_store: Arc<dyn ExperienceStore> = session_store_concrete.clone();
    let learning_candidate_store: Arc<dyn LearningCandidateStore> = session_store_concrete.clone();
    let rule_store: Arc<dyn ActionPolicyRuleStore> = session_store_concrete.clone();
    seed_memory(base_config, agent_config).await?;

    let tool_router = Arc::new(
        build_tool_router(
            base_config,
            session_store.clone(),
            rule_store,
            &workspace_dir,
            agent_config,
        )
        .await?,
    );

    let session_meta = SessionMeta {
        tenant_id,
        created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
        model: llm_provider.capabilities().model_id.clone(),
        title: Some(agent_config.name.clone()),
        ..SessionMeta::default()
    };
    let session_id = session_store.create_session(session_meta).await?;

    let pipeline = build_pipeline(
        base_config,
        agent_config,
        EvalPipelineDeps {
            session_store: session_store.clone(),
            segment_store: segment_store.clone(),
            graph_pool: session_store_concrete.pool().clone(),
            llm_provider: llm_provider.clone(),
            provider_registry: ProviderRegistry::from_config(base_config),
            lineage: lineage.clone(),
        },
        tool_router.as_ref(),
    )
    .await?;

    Ok(AgentEnvironment {
        session_store,
        segment_store,
        experience_store,
        learning_candidate_store,
        llm_provider,
        tool_router,
        pipeline,
        workspace_dir,
        session_id,
        storage_partition_id,
        user_id,
        lineage,
    })
}

async fn seed_memory(base_config: &MoaConfig, agent_config: &AgentConfig) -> Result<()> {
    if let Some(path) = &agent_config.memory.tenant_memory_path {
        return Err(EvalError::InvalidConfig(format!(
            "tenant memory fixture seeding is not implemented for {}",
            path.display()
        )));
    }
    if let Some(path) = &agent_config.memory.user_memory_path {
        return Err(EvalError::InvalidConfig(format!(
            "user memory fixture seeding is not implemented for {}",
            path.display()
        )));
    }
    if !agent_config.memory.clear_defaults
        && let Some(default_root) = configured_default_memory_root(base_config)?
    {
        let _ = default_root;
    }
    Ok(())
}

fn eval_tenant_id_for_agent(agent_name: &str) -> TenantId {
    tenant_id_from_label(&eval_storage_partition_id_for_agent(agent_name))
}

fn eval_storage_partition_id_for_agent(agent_name: &str) -> String {
    let mut slug = String::from("eval");
    let trimmed = agent_name.trim();
    if trimmed.is_empty() {
        return slug;
    }

    slug.push('-');
    for character in trimmed.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

async fn build_tool_router(
    base_config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    rule_store: Arc<dyn ActionPolicyRuleStore>,
    workspace_dir: &Path,
    agent_config: &AgentConfig,
) -> Result<ToolRouter> {
    let router = ToolRouter::new_local(workspace_dir).await?;
    let available_tools = router.tool_names();
    validate_named_tools(&available_tools, &agent_config.tools.disable)?;
    validate_named_tools(&available_tools, &agent_config.permissions.admin_review)?;
    validate_named_tools(&available_tools, &agent_config.permissions.always_deny)?;
    if let Some(enabled) = &agent_config.tools.enabled {
        validate_named_tools(&available_tools, enabled)?;
    }

    let enabled_tools = resolve_enabled_tools(&available_tools, agent_config);
    let policies = build_eval_policies(base_config, &agent_config.permissions, &enabled_tools)?;

    Ok(router
        .with_enabled_tools(enabled_tools)
        .with_rule_store(rule_store)
        .with_session_store(session_store)
        .with_policies(policies))
}

struct EvalPipelineDeps {
    session_store: Arc<dyn SessionStore>,
    segment_store: Arc<dyn SegmentStore>,
    graph_pool: sqlx::PgPool,
    llm_provider: Arc<dyn LLMProvider>,
    provider_registry: ProviderRegistry,
    lineage: Arc<dyn LineageHandle>,
}

async fn build_pipeline(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    deps: EvalPipelineDeps,
    tool_router: &ToolRouter,
) -> Result<ContextPipeline> {
    let workspace_instructions =
        load_workspace_instructions(base_config, &agent_config.instructions).await?;
    let user_instructions = base_config.general.user_instructions.clone();
    let tool_schemas = tool_router.tool_schemas();
    let query_rewrite_provider = resolve_eval_rewriter_provider(
        base_config,
        &deps.provider_registry,
        deps.llm_provider.clone(),
    );
    let mut eval_config = base_config.clone();
    eval_config.general.workspace_instructions = workspace_instructions;
    eval_config.general.user_instructions = user_instructions;

    Ok(
        build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
            &eval_config,
            deps.session_store,
            GraphMemoryPipelineOptions {
                graph_pool: deps.graph_pool,
                shared_graph_memory_retriever: None,
                retrieval_embedder: None,
                shared_skill_injector: None,
                segment_store: Some(deps.segment_store),
                compaction_llm_provider: Some(deps.llm_provider),
                query_rewrite_llm_provider: query_rewrite_provider,
                identity_prompt_override: Some(compose_identity_prompt(&agent_config.instructions)),
                tool_schemas,
                lineage: deps.lineage,
            },
        ),
    )
}

fn resolve_eval_rewriter_provider(
    base_config: &MoaConfig,
    provider_registry: &ProviderRegistry,
    fallback_provider: Arc<dyn LLMProvider>,
) -> Option<Arc<dyn LLMProvider>> {
    if !base_config.query_rewrite.enabled {
        return None;
    }

    if base_config.query_rewrite.model.is_some() || base_config.models.auxiliary.is_some() {
        let mut query_rewrite = base_config.query_rewrite.clone();
        if query_rewrite.model.is_none() {
            query_rewrite.model = base_config.models.auxiliary.clone();
        }
        match provider_registry.resolve_rewriter_provider(&query_rewrite) {
            Ok(Some(provider)) => return Some(provider),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to build eval query rewriter provider; falling back to main eval provider"
                );
            }
        }
    }

    Some(fallback_provider)
}

async fn load_workspace_instructions(
    base_config: &MoaConfig,
    instructions: &InstructionOverride,
) -> Result<Option<String>> {
    if let Some(path) = &instructions.workspace_instructions_path {
        let resolved = resolve_path(path)?;
        let text = fs::read_to_string(&resolved)
            .await
            .map_err(|source| EvalError::Io {
                path: resolved,
                source,
            })?;
        return Ok(Some(text));
    }

    Ok(base_config.general.workspace_instructions.clone())
}

fn compose_identity_prompt(instructions: &InstructionOverride) -> String {
    let mut prompt = instructions
        .system_prompt_override
        .clone()
        .unwrap_or_else(|| DEFAULT_IDENTITY_PROMPT.to_string());

    if let Some(extra) = instructions
        .system_prompt_append
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !prompt.trim().is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(extra);
    }

    prompt
}

fn build_eval_policies(
    base_config: &MoaConfig,
    permissions: &ActionPolicyOverride,
    enabled_tools: &[String],
) -> Result<ActionPolicies> {
    let mut config = base_config.clone();
    config.permissions.default_effect = permissions
        .default_effect
        .unwrap_or(ActionPolicyEffect::Allow);
    if enabled_tools.is_empty() {
        config.permissions.default_effect = ActionPolicyEffect::Deny;
    }
    config.permissions.admin_review = permissions.admin_review.clone();
    config.permissions.always_deny = permissions.always_deny.clone();
    Ok(ActionPolicies::from_config(&config)?)
}

fn resolve_enabled_tools(available_tools: &[String], agent_config: &AgentConfig) -> Vec<String> {
    let mut enabled = if let Some(explicit) = &agent_config.tools.enabled {
        explicit.clone()
    } else {
        available_tools.to_vec()
    };
    let disabled: HashSet<_> = agent_config.tools.disable.iter().cloned().collect();
    enabled.retain(|tool| !disabled.contains(tool));
    enabled.sort();
    enabled.dedup();
    enabled
}

fn validate_named_tools(available_tools: &[String], requested_tools: &[String]) -> Result<()> {
    let available = available_tools.iter().collect::<HashSet<_>>();
    for tool in requested_tools {
        if !available.contains(tool) {
            return Err(EvalError::InvalidConfig(format!(
                "unknown tool override '{tool}'"
            )));
        }
    }
    Ok(())
}

fn configured_default_memory_root(base_config: &MoaConfig) -> Result<Option<PathBuf>> {
    let configured_memory_dir = base_config
        .cloud
        .memory_dir
        .as_deref()
        .unwrap_or(&base_config.local.memory_dir);
    let memory_dir = expand_local_path(configured_memory_dir)?;
    Ok(memory_dir.parent().map(Path::to_path_buf))
}

fn resolve_path(path: &Path) -> Result<PathBuf> {
    let expanded = expand_local_path(path)?;
    if expanded.is_absolute() {
        return Ok(expanded);
    }

    let current_dir = std::env::current_dir().map_err(|source| EvalError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    Ok(current_dir.join(expanded))
}

fn expand_local_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let raw = path.to_string_lossy();
    if let Some(relative) = raw.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .map_err(|_| EvalError::Moa(moa_core::MoaError::HomeDirectoryNotFound))?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionResponse, CompletionStream, LLMProvider, MoaConfig,
        ModelCapabilities, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
    };
    use tempfile::tempdir;

    use super::{
        build_agent_environment_with_provider, eval_storage_partition_id_for_agent,
        eval_tenant_id_for_agent, seed_memory,
    };
    use moa_eval_core::{ActionPolicyOverride, AgentConfig, MemoryOverride, ToolOverride};

    fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens_uncached: input_tokens,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        }
    }

    #[derive(Clone)]
    struct MockProvider;

    #[async_trait]
    impl LLMProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: moa_core::ModelId::new("mock-model"),
                context_window: 32_000,
                max_output: 1_024,
                supports_tools: true,
                supports_vision: false,
                supports_prefix_caching: false,
                cache_ttl: None,
                tool_call_format: ToolCallFormat::Anthropic,
                pricing: TokenPricing {
                    input_per_mtok: 1.0,
                    output_per_mtok: 2.0,
                    cached_input_per_mtok: None,
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> moa_core::Result<CompletionStream> {
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "ok".to_string(),
                content: vec![moa_core::CompletionContent::Text("ok".to_string())],
                stop_reason: StopReason::EndTurn,
                model: moa_core::ModelId::new("mock-model"),
                usage: token_usage(1, 1),
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    #[test]
    fn eval_tenant_id_is_stable_for_agent_name() {
        // Pins: pre-seeded tenant-scoped eval artifacts use the same agent-name
        // storage partition as the runtime session.
        let first = eval_tenant_id_for_agent("Experience Learning Agent");
        let second = eval_tenant_id_for_agent("Experience Learning Agent");
        let slug = eval_storage_partition_id_for_agent("Experience Learning Agent");

        assert_eq!(first, second);
        assert_eq!(slug, "eval-experience-learning-agent");
    }

    #[tokio::test]
    async fn setup_db_respects_tool_allowlist() {
        let temp = tempdir().unwrap();
        let moa_config = test_moa_config();
        let config = AgentConfig {
            name: "test".to_string(),
            tools: ToolOverride {
                enabled: Some(vec!["file_read".to_string()]),
                ..ToolOverride::default()
            },
            permissions: ActionPolicyOverride::default(),
            ..AgentConfig::default()
        };

        let environment = build_agent_environment_with_provider(
            &moa_config,
            &config,
            temp.path(),
            Arc::new(MockProvider),
        )
        .await
        .unwrap();

        assert!(environment.tool_router.has_tool("file_read"));
        assert!(!environment.tool_router.has_tool("bash"));
        uuid::Uuid::parse_str(environment.storage_partition_id.as_str())
            .expect("eval tenant id should be the tenant UUID string");
    }

    #[tokio::test]
    async fn setup_rejects_unimplemented_memory_seed_paths() {
        // Pins: eval configs cannot silently ignore explicit memory fixture paths.
        let moa_config = test_moa_config();
        let config = AgentConfig {
            memory: MemoryOverride {
                tenant_memory_path: Some("tenant-memory.jsonl".into()),
                ..MemoryOverride::default()
            },
            ..AgentConfig::default()
        };

        let error = seed_memory(&moa_config, &config)
            .await
            .expect_err("tenant fixture path should fail until seeding is implemented");

        assert!(
            error
                .to_string()
                .contains("tenant memory fixture seeding is not implemented")
        );
    }

    fn test_moa_config() -> MoaConfig {
        let mut config = MoaConfig::default();
        if let Ok(url) = std::env::var("MOA_DATABASE_URL") {
            config.database.url = url;
        }
        config
    }
}
