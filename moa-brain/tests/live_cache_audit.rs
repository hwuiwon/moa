//! Live cache audit coverage for prompt caching behavior across turns, sessions, and model switches.

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::{build_default_pipeline_with_runtime_and_instructions, run_brain_turn_with_tools};
use moa_core::workspace::discover_workspace_instructions;
use moa_core::{
    CacheTtl, CompletionRequest, CompletionResponse, CompletionStream, ContextMessage, Event,
    EventRange, LLMProvider, MessageRole, MoaConfig, Result, SessionMeta, SessionStore,
    ToolContent, UserId, WorkspaceId, estimate_text_tokens,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_providers::build_provider_from_config;
use moa_session::testing;
use serde::Serialize;
use tempfile::tempdir;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
struct ToolSummary {
    index: usize,
    tokens_estimate: usize,
    fingerprint: u64,
}

#[derive(Debug, Clone, Serialize)]
struct MessageSummary {
    index: usize,
    role: String,
    tokens_estimate: usize,
    in_stable_prefix: bool,
    fingerprint: u64,
    preview: String,
}

#[derive(Debug, Clone, Serialize)]
struct CacheTurnAudit {
    scenario: String,
    turn_label: String,
    provider: String,
    model: String,
    tool_count: usize,
    message_count: usize,
    cache_breakpoints: Vec<usize>,
    tool_tokens_estimate: usize,
    stable_message_tokens_estimate: usize,
    stable_total_tokens_estimate: usize,
    total_tokens_estimate: usize,
    dynamic_tokens_estimate: usize,
    stable_prefix_fingerprint: u64,
    full_request_fingerprint: u64,
    request_tools: Vec<ToolSummary>,
    request_messages: Vec<MessageSummary>,
    input_tokens: usize,
    cached_input_tokens: usize,
    output_tokens: usize,
    cached_vs_stable_estimate_ratio: f64,
    stable_prefix_reused_from_previous_request: bool,
}

#[derive(Debug, Clone)]
struct CacheTurnPlan {
    scenario: String,
    turn_label: String,
    provider: String,
    model: String,
    tool_count: usize,
    message_count: usize,
    cache_breakpoints: Vec<usize>,
    tool_tokens_estimate: usize,
    stable_message_tokens_estimate: usize,
    stable_total_tokens_estimate: usize,
    total_tokens_estimate: usize,
    dynamic_tokens_estimate: usize,
    stable_prefix_fingerprint: u64,
    full_request_fingerprint: u64,
    request_tools: Vec<ToolSummary>,
    request_messages: Vec<MessageSummary>,
}

impl CacheTurnPlan {
    fn from_request(
        scenario: impl Into<String>,
        turn_label: impl Into<String>,
        provider: impl Into<String>,
        request: &CompletionRequest,
    ) -> Self {
        let scenario = scenario.into();
        let turn_label = turn_label.into();
        let provider = provider.into();
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| moa_core::ModelId::new("unspecified"));
        let stable_message_count = static_prefix_message_count(request);
        let request_tools = request
            .tools
            .iter()
            .enumerate()
            .map(|(index, schema)| ToolSummary {
                index,
                tokens_estimate: estimate_text_tokens(&schema.to_string()),
                fingerprint: stable_fingerprint(&schema.to_string()),
            })
            .collect::<Vec<_>>();
        let request_messages = request
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| MessageSummary {
                index,
                role: role_label(message.role.clone()),
                tokens_estimate: estimate_text_tokens(&message.content),
                in_stable_prefix: index < stable_message_count,
                fingerprint: stable_fingerprint(&serialized_message(message)),
                preview: preview_text(&message.content),
            })
            .collect::<Vec<_>>();
        let tool_tokens_estimate = request_tools
            .iter()
            .map(|tool| tool.tokens_estimate)
            .sum::<usize>();
        let stable_message_tokens_estimate = request_messages
            .iter()
            .filter(|message| message.in_stable_prefix)
            .map(|message| message.tokens_estimate)
            .sum::<usize>();
        let total_message_tokens_estimate = request_messages
            .iter()
            .map(|message| message.tokens_estimate)
            .sum::<usize>();
        let total_tokens_estimate = tool_tokens_estimate + total_message_tokens_estimate;
        let stable_total_tokens_estimate = tool_tokens_estimate + stable_message_tokens_estimate;
        let dynamic_tokens_estimate =
            total_tokens_estimate.saturating_sub(stable_total_tokens_estimate);
        let stable_prefix_fingerprint =
            stable_fingerprint(&stable_prefix_payload(request, stable_message_count));
        let full_request_fingerprint = stable_fingerprint(&full_request_payload(request));

        Self {
            scenario,
            turn_label,
            provider,
            model: model.to_string(),
            tool_count: request.tools.len(),
            message_count: request.messages.len(),
            cache_breakpoints: request.cache_breakpoints.clone(),
            tool_tokens_estimate,
            stable_message_tokens_estimate,
            stable_total_tokens_estimate,
            total_tokens_estimate,
            dynamic_tokens_estimate,
            stable_prefix_fingerprint,
            full_request_fingerprint,
            request_tools,
            request_messages,
        }
    }

    fn finalize(
        self,
        response: &CompletionResponse,
        stable_prefix_reused_from_previous_request: bool,
    ) -> CacheTurnAudit {
        let cached_vs_stable_estimate_ratio = if self.stable_total_tokens_estimate == 0 {
            0.0
        } else {
            response.cached_input_tokens as f64 / self.stable_total_tokens_estimate as f64
        };

        CacheTurnAudit {
            scenario: self.scenario,
            turn_label: self.turn_label,
            provider: self.provider,
            model: self.model,
            tool_count: self.tool_count,
            message_count: self.message_count,
            cache_breakpoints: self.cache_breakpoints,
            tool_tokens_estimate: self.tool_tokens_estimate,
            stable_message_tokens_estimate: self.stable_message_tokens_estimate,
            stable_total_tokens_estimate: self.stable_total_tokens_estimate,
            total_tokens_estimate: self.total_tokens_estimate,
            dynamic_tokens_estimate: self.dynamic_tokens_estimate,
            stable_prefix_fingerprint: self.stable_prefix_fingerprint,
            full_request_fingerprint: self.full_request_fingerprint,
            request_tools: self.request_tools,
            request_messages: self.request_messages,
            input_tokens: response.input_tokens,
            cached_input_tokens: response.cached_input_tokens,
            output_tokens: response.output_tokens,
            cached_vs_stable_estimate_ratio,
            stable_prefix_reused_from_previous_request,
        }
    }
}

#[derive(Clone)]
struct AuditedProvider {
    inner: Arc<dyn LLMProvider>,
    scenario: String,
    labels: Arc<Vec<String>>,
    audits: Arc<tokio::sync::Mutex<Vec<CacheTurnAudit>>>,
    previous_stable_prefix: Arc<tokio::sync::Mutex<Option<u64>>>,
}

#[tokio::test]
#[ignore = "requires provider API key env and performs live cache audits"]
async fn live_cache_audit_reports_hits_for_available_providers() -> Result<()> {
    let repo_root = repo_root()?;
    let dir = tempdir()?;

    let workspace_id = WorkspaceId::new("cache-audit-matrix");
    let user_id = UserId::new("cache-audit-user");
    let discovered_instructions = discover_workspace_instructions(&repo_root);

    let memory_store = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    let store = Arc::new(store);

    let provider_configs = available_live_cache_provider_configs(&repo_root);
    if provider_configs.is_empty() {
        return Ok(());
    }

    let mut audits_by_provider = serde_json::Map::new();

    for (provider_name, config) in provider_configs {
        let audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
            build_provider_from_config(&config)?,
            format!("same_session_{provider_name}"),
            vec![
                "warm_1".to_string(),
                "warm_2".to_string(),
                "warm_3".to_string(),
            ],
            audits.clone(),
        ));
        let pipeline = build_default_pipeline_with_runtime_and_instructions(
            &config,
            store.clone(),
            memory_store.clone(),
            Some(provider.clone()),
            discovered_instructions.clone(),
            Vec::new(),
        );

        let session_id = create_session(
            store.clone(),
            &workspace_id,
            &user_id,
            &config.general.default_model,
        )
        .await?;
        run_turn(
            store.clone(),
            session_id,
            provider.clone(),
            &pipeline,
            None,
            "Reply with READY and nothing else.",
        )
        .await?;
        run_turn(
            store.clone(),
            session_id,
            provider.clone(),
            &pipeline,
            None,
            "Reply with STEADY and nothing else.",
        )
        .await?;
        run_turn(
            store.clone(),
            session_id,
            provider,
            &pipeline,
            None,
            "Reply with STABLE and nothing else.",
        )
        .await?;

        let provider_audits = audits.lock().await.clone();
        audits_by_provider.insert(
            provider_name.clone(),
            serde_json::to_value(&provider_audits)?,
        );

        assert_eq!(
            provider_audits.len(),
            3,
            "expected three audit samples for {provider_name}"
        );
        assert!(
            provider_audits
                .get(1)
                .is_some_and(|audit| audit.stable_prefix_reused_from_previous_request),
            "expected turn 2 static-prefix reuse for {provider_name}"
        );
        assert!(
            provider_audits
                .iter()
                .skip(1)
                .any(|audit| audit.cached_input_tokens > 0),
            "expected a cache hit on turn 2 or 3 for {provider_name}: {provider_audits:#?}"
        );
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(audits_by_provider))?
    );

    Ok(())
}

impl AuditedProvider {
    fn new(
        inner: Arc<dyn LLMProvider>,
        scenario: impl Into<String>,
        labels: Vec<String>,
        audits: Arc<tokio::sync::Mutex<Vec<CacheTurnAudit>>>,
    ) -> Self {
        Self {
            inner,
            scenario: scenario.into(),
            labels: Arc::new(labels),
            audits,
            previous_stable_prefix: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[async_trait]
impl LLMProvider for AuditedProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let turn_index = self.audits.lock().await.len();
        let turn_label = self
            .labels
            .get(turn_index)
            .cloned()
            .unwrap_or_else(|| format!("turn_{}", turn_index + 1));
        let plan = CacheTurnPlan::from_request(
            self.scenario.clone(),
            turn_label,
            self.inner.name().to_string(),
            &request,
        );
        let response = self.inner.complete(request).await?.collect().await?;
        let mut previous = self.previous_stable_prefix.lock().await;
        let reused = previous
            .map(|fingerprint| fingerprint == plan.stable_prefix_fingerprint)
            .unwrap_or(false);
        *previous = Some(plan.stable_prefix_fingerprint);
        self.audits
            .lock()
            .await
            .push(plan.finalize(&response, reused));

        Ok(CompletionStream::from_response(response))
    }
}

#[tokio::test]
#[ignore = "requires provider API key env and performs live cache audits"]
async fn live_cache_audit_tracks_same_session_cross_session_and_model_switch() -> Result<()> {
    let repo_root = repo_root()?;
    let dir = tempdir()?;

    let workspace_id = WorkspaceId::new("cache-audit");
    let user_id = UserId::new("cache-audit-user");
    let discovered_instructions = discover_workspace_instructions(&repo_root);

    let mut sonnet_config = MoaConfig::default();
    sonnet_config.general.default_provider = "anthropic".to_string();
    sonnet_config.general.default_model = "claude-sonnet-4-6".to_string();
    sonnet_config.local.sandbox_dir = repo_root.display().to_string();

    let memory_store = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    let store = Arc::new(store);
    let tool_router = Arc::new(
        ToolRouter::from_config(&sonnet_config, memory_store.clone())
            .await?
            .with_rule_store(store.clone())
            .with_session_store(store.clone()),
    );
    tool_router
        .remember_workspace_root(workspace_id.clone(), repo_root.clone())
        .await;

    let same_session_audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let sonnet_provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
        build_provider_from_config(&sonnet_config)?,
        "same_session_sonnet",
        vec![
            "warm_1".to_string(),
            "warm_2".to_string(),
            "repo_task".to_string(),
        ],
        same_session_audits.clone(),
    ));
    let sonnet_pipeline = build_default_pipeline_with_runtime_and_instructions(
        &sonnet_config,
        store.clone(),
        memory_store.clone(),
        Some(sonnet_provider.clone()),
        discovered_instructions.clone(),
        tool_router.tool_schemas(),
    );

    let session_a =
        create_session(store.clone(), &workspace_id, &user_id, "claude-sonnet-4-6").await?;
    run_turn(
        store.clone(),
        session_a,
        sonnet_provider.clone(),
        &sonnet_pipeline,
        Some(tool_router.clone()),
        "Reply with READY and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        session_a,
        sonnet_provider.clone(),
        &sonnet_pipeline,
        Some(tool_router.clone()),
        "Reply with STEADY and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        session_a,
        sonnet_provider.clone(),
        &sonnet_pipeline,
        Some(tool_router.clone()),
        "In this repository, what is the package name in moa-brain/Cargo.toml? Use tools if needed and answer with just the value.",
    )
    .await?;

    let cross_session_audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let cross_session_provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
        build_provider_from_config(&sonnet_config)?,
        "cross_session_sonnet",
        vec!["fresh_session_repeat".to_string()],
        cross_session_audits.clone(),
    ));
    let cross_session_pipeline = build_default_pipeline_with_runtime_and_instructions(
        &sonnet_config,
        store.clone(),
        memory_store.clone(),
        Some(cross_session_provider.clone()),
        discovered_instructions.clone(),
        tool_router.tool_schemas(),
    );
    let session_b =
        create_session(store.clone(), &workspace_id, &user_id, "claude-sonnet-4-6").await?;
    run_turn(
        store.clone(),
        session_b,
        cross_session_provider.clone(),
        &cross_session_pipeline,
        Some(tool_router.clone()),
        "Reply with READY and nothing else.",
    )
    .await?;

    let cold_session_audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let cold_session_provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
        build_provider_from_config(&sonnet_config)?,
        "cold_prefix_sonnet",
        vec!["salted_cold".to_string(), "salted_warm".to_string()],
        cold_session_audits.clone(),
    ));
    let cold_instructions = salted_instructions(
        discovered_instructions.clone(),
        &format!("cache-audit-salt:{}", Uuid::now_v7()),
    );
    let cold_session_pipeline = build_default_pipeline_with_runtime_and_instructions(
        &sonnet_config,
        store.clone(),
        memory_store.clone(),
        Some(cold_session_provider.clone()),
        cold_instructions,
        tool_router.tool_schemas(),
    );
    let session_c =
        create_session(store.clone(), &workspace_id, &user_id, "claude-sonnet-4-6").await?;
    run_turn(
        store.clone(),
        session_c,
        cold_session_provider.clone(),
        &cold_session_pipeline,
        Some(tool_router.clone()),
        "Reply with COLD and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        session_c,
        cold_session_provider.clone(),
        &cold_session_pipeline,
        Some(tool_router.clone()),
        "Reply with WARM and nothing else.",
    )
    .await?;

    let mut opus_config = sonnet_config.clone();
    opus_config.general.default_model = "claude-opus-4-6".to_string();
    let model_switch_audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let opus_provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
        build_provider_from_config(&opus_config)?,
        "model_switch_opus",
        vec!["switch_cold".to_string(), "switch_warm".to_string()],
        model_switch_audits.clone(),
    ));
    let opus_pipeline = build_default_pipeline_with_runtime_and_instructions(
        &opus_config,
        store.clone(),
        memory_store.clone(),
        Some(opus_provider.clone()),
        discovered_instructions,
        tool_router.tool_schemas(),
    );
    run_turn(
        store.clone(),
        session_a,
        opus_provider.clone(),
        &opus_pipeline,
        Some(tool_router.clone()),
        "Reply with SWITCHED and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        session_a,
        opus_provider.clone(),
        &opus_pipeline,
        Some(tool_router),
        "Reply with SWITCHED2 and nothing else.",
    )
    .await?;

    let same_session_audits = same_session_audits.lock().await.clone();
    let cross_session_audits = cross_session_audits.lock().await.clone();
    let cold_session_audits = cold_session_audits.lock().await.clone();
    let model_switch_audits = model_switch_audits.lock().await.clone();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "same_session_sonnet": same_session_audits,
            "cross_session_sonnet": cross_session_audits,
            "cold_prefix_sonnet": cold_session_audits,
            "model_switch_opus": model_switch_audits,
        }))?
    );

    assert!(
        same_session_audits
            .get(1)
            .is_some_and(|audit| audit.stable_prefix_reused_from_previous_request),
        "expected the second Sonnet turn to reuse the same stable prefix plan"
    );
    assert!(
        same_session_audits
            .iter()
            .skip(1)
            .any(|audit| audit.cached_input_tokens > 0),
        "expected an eventual same-session cache hit once the stable prefix warmed"
    );
    assert!(
        cross_session_audits
            .first()
            .is_some_and(|audit| audit.cached_input_tokens > 0),
        "expected a fresh-session cache hit when repeating the same first prompt"
    );
    assert_eq!(
        cold_session_audits.len(),
        2,
        "expected exactly two salted cold-prefix audit samples"
    );
    assert!(
        cold_session_audits[1].stable_prefix_reused_from_previous_request,
        "expected the second salted turn to reuse the same stable prefix"
    );
    assert_ne!(
        cold_session_audits[0].stable_prefix_fingerprint,
        same_session_audits[0].stable_prefix_fingerprint,
        "expected the salted scenario to produce a distinct stable prefix"
    );
    assert!(
        model_switch_audits
            .get(1)
            .is_some_and(|audit| audit.stable_prefix_reused_from_previous_request),
        "expected the second Opus turn to reuse the same stable prefix plan"
    );

    Ok(())
}

async fn create_session(
    store: Arc<dyn SessionStore>,
    workspace_id: &WorkspaceId,
    user_id: &UserId,
    model: &str,
) -> Result<moa_core::SessionId> {
    store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: user_id.clone(),
            model: moa_core::ModelId::new(model),
            ..SessionMeta::default()
        })
        .await
}

async fn run_turn(
    store: Arc<dyn SessionStore>,
    session_id: moa_core::SessionId,
    provider: Arc<dyn LLMProvider>,
    pipeline: &moa_brain::ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    prompt: &str,
) -> Result<()> {
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: prompt.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    let result =
        run_brain_turn_with_tools(session_id, store.clone(), provider, pipeline, tool_router)
            .await?;

    assert_eq!(result, moa_brain::TurnResult::Complete);
    let _events = store.get_events(session_id, EventRange::all()).await?;
    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    for candidate in cwd.ancestors() {
        if is_repo_root(candidate) {
            return Ok(candidate.to_path_buf());
        }
    }

    Err(moa_core::MoaError::ValidationError(format!(
        "could not locate repo root from {}",
        cwd.display()
    )))
}

fn is_repo_root(path: &Path) -> bool {
    path.join("Cargo.toml").exists() && path.join("moa-brain").exists()
}

fn salted_instructions(base: Option<String>, salt: &str) -> Option<String> {
    let salt_block = format!("\n\n<!-- {salt} -->\n");
    match base {
        Some(existing) => Some(format!("{existing}{salt_block}")),
        None => Some(salt_block),
    }
}

fn role_label(role: MessageRole) -> String {
    match role {
        MessageRole::System => "system".to_string(),
        MessageRole::User => "user".to_string(),
        MessageRole::Assistant => "assistant".to_string(),
        MessageRole::Tool => "tool".to_string(),
    }
}

fn available_live_cache_provider_configs(repo_root: &Path) -> Vec<(String, MoaConfig)> {
    let mut configs = Vec::new();

    if env::var("ANTHROPIC_API_KEY").is_ok() {
        let mut config = live_cache_config("anthropic", "claude-sonnet-4-6", repo_root);
        config.providers.anthropic.api_key_env = "ANTHROPIC_API_KEY".to_string();
        configs.push(("anthropic".to_string(), config));
    }
    if env::var("OPENAI_API_KEY").is_ok() {
        let mut config = live_cache_config("openai", "gpt-5.4", repo_root);
        config.providers.openai.api_key_env = "OPENAI_API_KEY".to_string();
        configs.push(("openai".to_string(), config));
    }
    if env::var("GOOGLE_API_KEY").is_ok() {
        let mut config = live_cache_config("google", "gemini-2.5-flash", repo_root);
        config.providers.google.api_key_env = "GOOGLE_API_KEY".to_string();
        configs.push(("google".to_string(), config));
    }

    configs
}

fn live_cache_config(provider: &str, model: &str, repo_root: &Path) -> MoaConfig {
    let mut config = MoaConfig::default();
    config.general.default_provider = provider.to_string();
    config.general.default_model = model.to_string();
    config.general.workspace_instructions =
        Some("Cache audit static padding. Keep this prefix identical across turns.\n".repeat(220));
    config.local.sandbox_dir = repo_root.display().to_string();
    config
}

fn serialized_message(message: &ContextMessage) -> String {
    let blocks = message
        .content_blocks
        .as_ref()
        .map(|blocks| {
            blocks
                .iter()
                .map(serialized_tool_content)
                .collect::<Vec<_>>()
                .join("|")
        })
        .unwrap_or_default();
    format!(
        "{}:{}:{}:{}",
        role_label(message.role.clone()),
        message.content,
        message.tool_use_id.clone().unwrap_or_default(),
        blocks
    )
}

fn serialized_tool_content(content: &ToolContent) -> String {
    match content {
        ToolContent::Text { text } => format!("text:{text}"),
        ToolContent::Json { data } => format!("json:{data}"),
    }
}

fn stable_prefix_payload(request: &CompletionRequest, stable_message_count: usize) -> String {
    let mut segments = request
        .tools
        .iter()
        .map(|tool| format!("tool:{}", tool))
        .collect::<Vec<_>>();
    segments.extend(
        request
            .messages
            .iter()
            .take(stable_message_count)
            .map(serialized_message)
            .map(|message| format!("message:{message}")),
    );
    segments.join("\n")
}

fn static_prefix_message_count(request: &CompletionRequest) -> usize {
    request
        .cache_controls
        .iter()
        .filter(|breakpoint| breakpoint.ttl == CacheTtl::OneHour)
        .filter_map(moa_core::CacheBreakpoint::message_index)
        .max()
        .or_else(|| request.cache_breakpoints.last().copied())
        .unwrap_or_default()
        .min(request.messages.len())
}

fn full_request_payload(request: &CompletionRequest) -> String {
    let mut segments = request
        .tools
        .iter()
        .map(|tool| format!("tool:{}", tool))
        .collect::<Vec<_>>();
    segments.extend(
        request
            .messages
            .iter()
            .map(serialized_message)
            .map(|message| format!("message:{message}")),
    );
    segments.join("\n")
}

fn stable_fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn preview_text(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 96;

    let preview = text.trim().replace('\n', "\\n");
    if preview.chars().count() <= MAX_PREVIEW_CHARS {
        return preview;
    }

    let truncated = preview.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
    format!("{truncated}...")
}
