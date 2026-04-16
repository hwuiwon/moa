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
        memory::MemoryRetriever,
        runtime_context::RuntimeContextProcessor,
        skills::SkillInjector,
        tools::ToolDefinitionProcessor,
    },
};
use moa_core::{
    ContextProcessor, LLMProvider, MemoryPath, MemoryScope, MemoryStore, MoaConfig, PageType,
    SessionMeta, SessionStore, UserId, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_memory::wiki::parse_markdown;
use moa_providers::{build_provider_from_selection, resolve_provider_selection};
use moa_security::{ApprovalRuleStore, ToolPolicies};
use moa_session::PostgresSessionStore;
use serde_json::Value;
use tokio::fs;
use uuid::Uuid;

use crate::{AgentConfig, EvalError, PermissionOverride, Result};

const DEFAULT_EVAL_WORKSPACE: &str = "eval";
const DEFAULT_EVAL_USER: &str = "eval-runner";

/// Fully isolated runtime environment for one eval execution.
pub struct AgentEnvironment {
    /// Session store scoped to this run.
    pub session_store: Arc<dyn SessionStore>,
    /// Memory store scoped to this run.
    pub memory_store: Arc<dyn MemoryStore>,
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
    let memory_root = run_root.join("memory-store");
    fs::create_dir_all(&workspace_dir)
        .await
        .map_err(|source| EvalError::Io {
            path: workspace_dir.clone(),
            source,
        })?;
    fs::create_dir_all(&memory_root)
        .await
        .map_err(|source| EvalError::Io {
            path: memory_root.clone(),
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
    let memory_store_concrete = Arc::new(FileMemoryStore::new(&memory_root).await?);
    seed_memory(
        base_config,
        agent_config,
        memory_store_concrete.as_ref(),
        &workspace_id,
    )
    .await?;

    let tool_router = Arc::new(
        build_tool_router(
            base_config,
            memory_store_concrete.clone(),
            session_store.clone(),
            rule_store,
            &workspace_dir,
            agent_config,
        )
        .await?,
    );

    if tool_router.has_tool("memory_read") {
        apply_skill_overrides(memory_store_concrete.as_ref(), agent_config, &workspace_id).await?;
    } else {
        clear_workspace_skills(memory_store_concrete.as_ref(), &workspace_id).await?;
    }

    refresh_indices(memory_store_concrete.as_ref(), &workspace_id).await?;

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
        memory_store_concrete.clone(),
        llm_provider.clone(),
        tool_router.as_ref(),
    )
    .await?;

    Ok(AgentEnvironment {
        session_store,
        memory_store: memory_store_concrete,
        llm_provider,
        tool_router,
        pipeline,
        workspace_dir,
        session_id,
        workspace_id,
        user_id,
    })
}

async fn seed_memory(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    memory_store: &FileMemoryStore,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    if !agent_config.memory.clear_defaults
        && let Some(default_root) = configured_default_memory_root(base_config)?
    {
        copy_dir_contents_if_exists(
            &default_root.join("memory"),
            &memory_store.base_dir().join("memory"),
        )
        .await?;
        copy_dir_contents_if_exists(
            &default_root
                .join("workspaces")
                .join(workspace_id.as_str())
                .join("memory"),
            &memory_store
                .base_dir()
                .join("workspaces")
                .join(workspace_id.as_str())
                .join("memory"),
        )
        .await?;
    }

    if let Some(snapshot) = &agent_config.memory.user_memory_path {
        copy_dir_contents(
            &resolve_path(snapshot)?,
            &memory_store.base_dir().join("memory"),
        )
        .await?;
    }

    if let Some(snapshot) = &agent_config.memory.workspace_memory_path {
        copy_dir_contents(
            &resolve_path(snapshot)?,
            &memory_store
                .base_dir()
                .join("workspaces")
                .join(workspace_id.as_str())
                .join("memory"),
        )
        .await?;
    }

    Ok(())
}

async fn build_tool_router(
    base_config: &MoaConfig,
    memory_store: Arc<FileMemoryStore>,
    session_store: Arc<dyn SessionStore>,
    rule_store: Arc<dyn ApprovalRuleStore>,
    workspace_dir: &Path,
    agent_config: &AgentConfig,
) -> Result<ToolRouter> {
    let memory_store_dyn: Arc<dyn MemoryStore> = memory_store;
    let router = ToolRouter::new_local(memory_store_dyn, workspace_dir).await?;
    let available_tools = router.tool_names();
    validate_named_tools(&available_tools, &agent_config.tools.disable)?;
    validate_named_tools(&available_tools, &agent_config.permissions.auto_approve)?;
    validate_named_tools(&available_tools, &agent_config.permissions.always_deny)?;
    if let Some(enabled) = &agent_config.tools.enabled {
        validate_named_tools(&available_tools, enabled)?;
    }

    let enabled_tools = resolve_enabled_tools(&available_tools, agent_config)?;
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
    memory_store: Arc<FileMemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: &ToolRouter,
) -> Result<ContextPipeline> {
    let identity_prompt = compose_identity_prompt(&agent_config.instructions);
    let workspace_instructions =
        load_workspace_instructions(base_config, &agent_config.instructions).await?;
    let user_instructions = base_config.general.user_instructions.clone();
    let tool_schemas = tool_router.tool_schemas();
    let memory_store_dyn: Arc<dyn MemoryStore> = memory_store.clone();
    let mut stages: Vec<Box<dyn ContextProcessor>> = vec![
        Box::new(IdentityProcessor::new(identity_prompt)),
        Box::new(InstructionProcessor::new(
            workspace_instructions,
            user_instructions,
            None,
        )),
        Box::new(ToolDefinitionProcessor::new(tool_schemas)),
        Box::new(SkillInjector::from_memory(memory_store_dyn.clone())),
        Box::new(MemoryRetriever::new(memory_store_dyn)),
        Box::new(HistoryCompiler::with_compaction(
            session_store,
            llm_provider,
            base_config.compaction.clone(),
        )),
        Box::new(RuntimeContextProcessor::default()),
        Box::new(CacheOptimizer),
    ];

    if !tool_router.has_tool("memory_read") {
        stages.retain(|stage| stage.name() != "skills");
    }

    Ok(ContextPipeline::new(stages))
}

async fn apply_skill_overrides(
    memory_store: &FileMemoryStore,
    agent_config: &AgentConfig,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    if agent_config.skills.exclusive {
        clear_workspace_skills(memory_store, workspace_id).await?;
    }

    for skill_path in &agent_config.skills.include {
        let (page_path, page) = load_skill_page(skill_path).await?;
        memory_store
            .write_page(
                MemoryScope::Workspace(workspace_id.clone()),
                &page_path,
                page,
            )
            .await?;
    }

    if !agent_config.skills.exclude.is_empty() {
        let scope = MemoryScope::Workspace(workspace_id.clone());
        let summaries = memory_store
            .list_pages(scope.clone(), Some(moa_core::PageType::Skill))
            .await?;
        for summary in summaries {
            let page = memory_store.read_page(scope.clone(), &summary.path).await?;
            let skill_name = skill_name_from_page(&page).unwrap_or_else(|| page.title.clone());
            if agent_config
                .skills
                .exclude
                .iter()
                .any(|selector| skill_selector_matches(selector, &summary.path, &skill_name))
            {
                memory_store
                    .delete_page(scope.clone(), &summary.path)
                    .await?;
            }
        }
    }

    Ok(())
}

async fn clear_workspace_skills(
    memory_store: &FileMemoryStore,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    let scope = MemoryScope::Workspace(workspace_id.clone());
    let summaries = memory_store
        .list_pages(scope.clone(), Some(moa_core::PageType::Skill))
        .await?;
    for summary in summaries {
        memory_store
            .delete_page(scope.clone(), &summary.path)
            .await?;
    }
    Ok(())
}

async fn refresh_indices(memory_store: &FileMemoryStore, workspace_id: &WorkspaceId) -> Result<()> {
    let user_scope = MemoryScope::User(UserId::new(DEFAULT_EVAL_USER));
    let workspace_scope = MemoryScope::Workspace(workspace_id.clone());
    memory_store.refresh_scope_index(&user_scope).await?;
    memory_store.refresh_scope_index(&workspace_scope).await?;
    memory_store.rebuild_search_index(user_scope).await?;
    memory_store.rebuild_search_index(workspace_scope).await?;
    Ok(())
}

async fn load_skill_page(selector: &str) -> Result<(MemoryPath, moa_core::WikiPage)> {
    let resolved = resolve_path(Path::new(selector))?;
    let file_path = if resolved.is_dir() {
        resolved.join("SKILL.md")
    } else {
        resolved
    };
    let hinted_name = file_path
        .parent()
        .and_then(Path::file_name)
        .or_else(|| file_path.file_stem())
        .and_then(|value| value.to_str())
        .unwrap_or("skill");
    let hinted_path = MemoryPath::new(format!("skills/{}/SKILL.md", slugify_name(hinted_name)));
    let markdown = fs::read_to_string(&file_path)
        .await
        .map_err(|source| EvalError::Io {
            path: file_path.clone(),
            source,
        })?;
    let mut page = parse_markdown(Some(hinted_path), &markdown).map_err(EvalError::Moa)?;
    page.page_type = PageType::Skill;
    let path = build_skill_memory_path(&page, &file_path);
    page.path = Some(path.clone());
    Ok((path, page))
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

fn resolve_enabled_tools(
    available_tools: &[String],
    agent_config: &AgentConfig,
) -> Result<Vec<String>> {
    let mut enabled = if let Some(explicit) = &agent_config.tools.enabled {
        explicit.clone()
    } else {
        available_tools.to_vec()
    };
    let disabled: HashSet<_> = agent_config.tools.disable.iter().cloned().collect();
    enabled.retain(|tool| !disabled.contains(tool));
    enabled.sort();
    enabled.dedup();
    Ok(enabled)
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

async fn copy_dir_contents_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if !fs::try_exists(source)
        .await
        .map_err(|source_error| EvalError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?
    {
        return Ok(());
    }

    copy_dir_contents(source, destination).await
}

async fn copy_dir_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .await
        .map_err(|source_error| EvalError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })?;

    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        let mut entries = fs::read_dir(&src_dir)
            .await
            .map_err(|source_error| EvalError::Io {
                path: src_dir.clone(),
                source: source_error,
            })?;
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .map_err(|source_error| EvalError::Io {
                    path: src_dir.clone(),
                    source: source_error,
                })?
        {
            let entry_type = entry
                .file_type()
                .await
                .map_err(|source_error| EvalError::Io {
                    path: entry.path(),
                    source: source_error,
                })?;
            let target = dst_dir.join(entry.file_name());
            if entry_type.is_dir() {
                fs::create_dir_all(&target)
                    .await
                    .map_err(|source_error| EvalError::Io {
                        path: target.clone(),
                        source: source_error,
                    })?;
                stack.push((entry.path(), target));
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|source_error| EvalError::Io {
                            path: parent.to_path_buf(),
                            source: source_error,
                        })?;
                }
                fs::copy(entry.path(), &target)
                    .await
                    .map_err(|source_error| EvalError::Io {
                        path: target.clone(),
                        source: source_error,
                    })?;
            }
        }
    }

    Ok(())
}

fn skill_selector_matches(selector: &str, path: &MemoryPath, name: &str) -> bool {
    selector == name
        || selector == path.as_str()
        || path.as_str().contains(selector)
        || name.contains(selector)
}

fn skill_name_from_page(page: &moa_core::WikiPage) -> Option<String> {
    page.metadata
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn build_skill_memory_path(page: &moa_core::WikiPage, file_path: &Path) -> MemoryPath {
    let name = skill_name_from_page(page)
        .or_else(|| {
            file_path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| page.title.clone());
    MemoryPath::new(format!("skills/{}/SKILL.md", slugify_name(&name)))
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
        CompletionRequest, CompletionResponse, CompletionStream, LLMProvider, MemoryPath,
        MemoryScope, MoaConfig, ModelCapabilities, StopReason, TokenPricing, TokenUsage,
        ToolCallFormat,
    };
    use tempfile::tempdir;

    use super::{build_agent_environment_with_provider, slugify_name};
    use crate::{AgentConfig, MemoryOverride, PermissionOverride, ToolOverride};

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
                model_id: "mock-model".to_string(),
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
                model: "mock-model".to_string(),
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 0,
                usage: token_usage(1, 1),
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    #[tokio::test]
    async fn setup_respects_tool_allowlist() {
        let temp = tempdir().unwrap();
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
            &MoaConfig::default(),
            &config,
            temp.path(),
            Arc::new(MockProvider),
        )
        .await
        .unwrap();

        assert!(environment.tool_router.has_tool("file_read"));
        assert!(!environment.tool_router.has_tool("bash"));
    }

    #[tokio::test]
    async fn setup_copies_workspace_memory_snapshot() {
        let fixture = tempdir().unwrap();
        let snapshot_root = fixture.path().join("workspace");
        tokio::fs::create_dir_all(&snapshot_root).await.unwrap();
        tokio::fs::write(
            snapshot_root.join("notes.md"),
            "# Notes\n\nSnapshot content.",
        )
        .await
        .unwrap();

        let temp = tempdir().unwrap();
        let config = AgentConfig {
            name: "snapshot".to_string(),
            memory: MemoryOverride {
                workspace_memory_path: Some(snapshot_root),
                clear_defaults: true,
                ..MemoryOverride::default()
            },
            permissions: PermissionOverride {
                auto_approve_all: true,
                ..PermissionOverride::default()
            },
            ..AgentConfig::default()
        };

        let environment = build_agent_environment_with_provider(
            &MoaConfig::default(),
            &config,
            temp.path(),
            Arc::new(MockProvider),
        )
        .await
        .unwrap();

        let page = environment
            .memory_store
            .read_page(
                MemoryScope::Workspace(environment.workspace_id.clone()),
                &MemoryPath::new("notes.md"),
            )
            .await
            .unwrap();
        assert!(page.content.contains("Snapshot content."));
    }

    #[test]
    fn slugify_preserves_eval_prefix() {
        assert_eq!(slugify_name("Deploy Variant"), "eval-deploy-variant");
    }
}
