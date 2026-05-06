//! Isolated environment construction for eval runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moa_brain::{
    ContextPipeline,
    pipeline::{
        cache::CacheOptimizer,
        history::HistoryCompiler,
        identity::{DEFAULT_IDENTITY_PROMPT, IdentityProcessor},
        instructions::InstructionProcessor,
        memory::GraphMemoryRetriever,
        query_rewrite::QueryRewriter,
        runtime_context::RuntimeContextProcessor,
        tools::ToolDefinitionProcessor,
    },
};
use moa_core::{
    ContextProcessor, LLMProvider, MoaConfig, SessionMeta, SessionStore, UserId, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_providers::{
    build_provider_from_selection, resolve_provider_selection, resolve_rewriter_provider,
};
use moa_security::{ApprovalRuleStore, ToolPolicies};
use moa_session::PostgresSessionStore;
use tokio::fs;
use uuid::Uuid;

use crate::{AgentConfig, EvalError, PermissionOverride, Result};

const DEFAULT_EVAL_WORKSPACE: &str = "eval";
const DEFAULT_EVAL_USER: &str = "eval-runner";

/// Fully isolated runtime environment for one eval execution.
pub struct AgentEnvironment {
    /// Session store scoped to this run.
    pub session_store: Arc<dyn SessionStore>,
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
    /// Workspace identifier used inside the run.
    pub workspace_id: WorkspaceId,
    /// User identifier used inside the run.
    pub user_id: UserId,
}

/// Builds a complete isolated agent environment from an agent config.
pub async fn build_agent_environment(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    temp_dir: &Path,
) -> Result<AgentEnvironment> {
    let selection = resolve_provider_selection(base_config, agent_config.model.as_deref())?;
    let llm_provider = build_provider_from_selection(base_config, &selection)?;
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

    let workspace_id = WorkspaceId::new(slugify_name(&agent_config.name).as_str());
    let user_id = UserId::new(DEFAULT_EVAL_USER);
    let schema_name = format!("eval_{}", Uuid::now_v7().simple());
    let session_store_concrete = Arc::new(
        PostgresSessionStore::new_in_schema(&base_config.database.url, &schema_name).await?,
    );
    let session_store: Arc<dyn SessionStore> = session_store_concrete.clone();
    let rule_store: Arc<dyn ApprovalRuleStore> = session_store_concrete.clone();
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
        workspace_id: workspace_id.clone(),
        user_id: user_id.clone(),
        model: llm_provider.capabilities().model_id.clone(),
        title: Some(agent_config.name.clone()),
        ..SessionMeta::default()
    };
    let session_id = session_store.create_session(session_meta).await?;

    let pipeline = build_pipeline(
        base_config,
        agent_config,
        session_store.clone(),
        session_store_concrete.pool().clone(),
        llm_provider.clone(),
        tool_router.as_ref(),
    )
    .await?;

    Ok(AgentEnvironment {
        session_store,
        llm_provider,
        tool_router,
        pipeline,
        workspace_dir,
        session_id,
        workspace_id,
        user_id,
    })
}

async fn seed_memory(base_config: &MoaConfig, agent_config: &AgentConfig) -> Result<()> {
    if !agent_config.memory.clear_defaults
        && let Some(default_root) = configured_default_memory_root(base_config)?
    {
        let _ = default_root;
    }
    let _ = &agent_config.memory.user_memory_path;
    let _ = &agent_config.memory.workspace_memory_path;
    Ok(())
}

async fn build_tool_router(
    base_config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    rule_store: Arc<dyn ApprovalRuleStore>,
    workspace_dir: &Path,
    agent_config: &AgentConfig,
) -> Result<ToolRouter> {
    let router = ToolRouter::new_local(workspace_dir).await?;
    let available_tools = router.tool_names();
    validate_named_tools(&available_tools, &agent_config.tools.disable)?;
    validate_named_tools(&available_tools, &agent_config.permissions.auto_approve)?;
    validate_named_tools(&available_tools, &agent_config.permissions.always_deny)?;
    if let Some(enabled) = &agent_config.tools.enabled {
        validate_named_tools(&available_tools, enabled)?;
    }

    let enabled_tools = resolve_enabled_tools(&available_tools, agent_config);
    let policies = build_eval_policies(base_config, &agent_config.permissions, &enabled_tools);

    Ok(router
        .with_enabled_tools(enabled_tools)
        .with_rule_store(rule_store)
        .with_session_store(session_store)
        .with_policies(policies))
}

async fn build_pipeline(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    session_store: Arc<dyn SessionStore>,
    graph_pool: sqlx::PgPool,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: &ToolRouter,
) -> Result<ContextPipeline> {
    let identity_prompt = compose_identity_prompt(&agent_config.instructions);
    let workspace_instructions =
        load_workspace_instructions(base_config, &agent_config.instructions).await?;
    let user_instructions = base_config.general.user_instructions.clone();
    let tool_schemas = tool_router.tool_schemas();
    let query_rewrite_provider = resolve_eval_rewriter_provider(base_config, llm_provider.clone());
    let mut stages: Vec<Box<dyn ContextProcessor>> = vec![
        Box::new(IdentityProcessor::new(identity_prompt)),
        Box::new(InstructionProcessor::new(
            workspace_instructions,
            user_instructions,
            None,
        )),
        Box::new(ToolDefinitionProcessor::new(tool_schemas)),
    ];
    if let Some(query_rewrite_provider) = query_rewrite_provider {
        stages.push(Box::new(
            QueryRewriter::new(base_config.query_rewrite.clone(), query_rewrite_provider)
                .with_session_store(session_store.clone()),
        ));
    }
    stages.extend([
        Box::new(GraphMemoryRetriever::new(graph_pool)) as Box<dyn ContextProcessor>,
        Box::new(HistoryCompiler::with_compaction(
            session_store.clone(),
            llm_provider,
            base_config.compaction.clone(),
        )) as Box<dyn ContextProcessor>,
        Box::new(RuntimeContextProcessor::default()) as Box<dyn ContextProcessor>,
        Box::new(CacheOptimizer) as Box<dyn ContextProcessor>,
    ]);

    Ok(ContextPipeline::new(stages))
}

fn resolve_eval_rewriter_provider(
    base_config: &MoaConfig,
    fallback_provider: Arc<dyn LLMProvider>,
) -> Option<Arc<dyn LLMProvider>> {
    if !base_config.query_rewrite.enabled {
        return None;
    }

    if base_config.query_rewrite.model.is_some() || base_config.models.auxiliary.is_some() {
        match resolve_rewriter_provider(base_config) {
            Ok(provider) => return Some(provider),
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
    instructions: &crate::InstructionOverride,
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

fn compose_identity_prompt(instructions: &crate::InstructionOverride) -> String {
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
    permissions: &PermissionOverride,
    enabled_tools: &[String],
) -> ToolPolicies {
    let mut config = base_config.clone();
    config.permissions.auto_approve = if permissions.auto_approve_all {
        enabled_tools.to_vec()
    } else {
        permissions.auto_approve.clone()
    };
    config.permissions.always_deny = permissions.always_deny.clone();
    ToolPolicies::from_config(&config)
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
    let configured_memory_dir = if base_config.cloud.enabled {
        base_config
            .cloud
            .memory_dir
            .as_deref()
            .unwrap_or(&base_config.local.memory_dir)
    } else {
        &base_config.local.memory_dir
    };
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

fn slugify_name(name: &str) -> String {
    let mut slug = String::from(DEFAULT_EVAL_WORKSPACE);
    let trimmed = name.trim();
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionResponse, CompletionStream, LLMProvider, MoaConfig,
        ModelCapabilities, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
    };
    use tempfile::tempdir;

    use super::{build_agent_environment_with_provider, slugify_name};
    use crate::{AgentConfig, PermissionOverride, ToolOverride};

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

    #[tokio::test]
    async fn setup_respects_tool_allowlist() {
        let temp = tempdir().unwrap();
        let moa_config = test_moa_config();
        let config = AgentConfig {
            name: "test".to_string(),
            tools: ToolOverride {
                enabled: Some(vec!["file_read".to_string()]),
                ..ToolOverride::default()
            },
            permissions: PermissionOverride {
                auto_approve_all: true,
                ..PermissionOverride::default()
            },
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
    }

    #[test]
    fn slugify_preserves_eval_prefix() {
        assert_eq!(slugify_name("Deploy Variant"), "eval-deploy-variant");
    }

    fn test_moa_config() -> MoaConfig {
        let mut config = MoaConfig::default();
        if let Ok(url) =
            std::env::var("TEST_DATABASE_URL").or_else(|_| std::env::var("DATABASE_URL"))
        {
            config.database.url = url;
        }
        config
    }
}
