// Live counterpart: see cache_audit_offline.rs for the wiremock version that runs in PR CI.

//! Live cache audit coverage for prompt caching behavior across turns, sessions, and model switches.

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::{
    BrainTurnRequest, GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions, run_brain_turn,
};
use moa_config::MoaConfig;
use moa_core::{
    error::Result, events::Event, traits::LLMProvider, traits::SessionStore,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::CompletionStream, types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::contact::SessionActorRef,
    types::context::ContextMessage, types::context::MessageRole,
    types::context::estimate_text_tokens, types::events_stream::EventRange,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::UserId, types::session::SessionMeta, types::tools::ToolContent,
};
use moa_hands::ToolRouter;
use moa_providers::build_provider_from_config;
use moa_session::testing;
use serde::Serialize;
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
            .unwrap_or_else(|| moa_core::types::identifiers::ModelId::new("unspecified"));
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
        let usage = response.token_usage();
        let cached_vs_stable_estimate_ratio = if self.stable_total_tokens_estimate == 0 {
            0.0
        } else {
            usage.input_tokens_cache_read as f64 / self.stable_total_tokens_estimate as f64
        };

        CacheTurnAudit {
            scenario: self.scenario,
            turn_label: self.turn_label,
            provider: self.provider,
            model: self.model,
            tool_count: self.tool_count,
            message_count: self.message_count,
            tool_tokens_estimate: self.tool_tokens_estimate,
            stable_message_tokens_estimate: self.stable_message_tokens_estimate,
            stable_total_tokens_estimate: self.stable_total_tokens_estimate,
            total_tokens_estimate: self.total_tokens_estimate,
            dynamic_tokens_estimate: self.dynamic_tokens_estimate,
            stable_prefix_fingerprint: self.stable_prefix_fingerprint,
            full_request_fingerprint: self.full_request_fingerprint,
            request_tools: self.request_tools,
            request_messages: self.request_messages,
            input_tokens: usage.total_input_tokens(),
            cached_input_tokens: usage.input_tokens_cache_read,
            output_tokens: usage.output_tokens,
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, provider API key env, and performs live cache audits"]
async fn live_cache_audit_reports_hits_for_available_providers() -> Result<()> {
    let repo_root = repo_root()?;

    let storage_partition_id = StoragePartitionId::new("cache-audit-matrix");
    let user_id = UserId::new("cache-audit-user");

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
        let expects_cache_hits = provider.capabilities().supports_prefix_caching;
        let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
            &config,
            store.clone(),
            GraphMemoryPipelineOptions {
                graph_pool: store.pool().clone(),
                kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
                shared_graph_memory_retriever: None,
                retrieval_embedder: None,
                shared_skill_injector: None,
                segment_store: Some(store.clone()),
                compaction_llm_provider: Some(provider.clone()),
                query_rewrite_llm_provider: Some(provider.clone()),
                identity_prompt_override: None,
                tool_schemas: Vec::new(),
                lineage: Arc::new(moa_core::traits::NullLineageHandle),
            },
        );

        let session_id = create_session(
            store.clone(),
            &storage_partition_id,
            &user_id,
            &config.models.main,
        )
        .await?;
        run_turn(
            store.clone(),
            test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
            session_id,
            provider.clone(),
            &pipeline,
            None,
            "Reply with READY and nothing else.",
        )
        .await?;
        run_turn(
            store.clone(),
            test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
            session_id,
            provider.clone(),
            &pipeline,
            None,
            "Reply with STEADY and nothing else.",
        )
        .await?;
        run_turn(
            store.clone(),
            test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
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
        if expects_cache_hits {
            assert!(
                provider_audits
                    .iter()
                    .skip(1)
                    .any(|audit| audit.cached_input_tokens > 0),
                "expected a cache hit on turn 2 or 3 for {provider_name}: {provider_audits:#?}"
            );
        }
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

    fn capabilities(&self) -> moa_core::types::model::ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let audited_request = !is_query_rewrite_request(&request);
        let turn_index = self.audits.lock().await.len();
        let turn_label = self
            .labels
            .get(turn_index)
            .cloned()
            .unwrap_or_else(|| format!("turn_{}", turn_index + 1));
        let plan = audited_request.then(|| {
            CacheTurnPlan::from_request(
                self.scenario.clone(),
                turn_label,
                self.inner.name().to_string(),
                &request,
            )
        });
        let response = self.inner.complete(request).await?.collect().await?;

        if let Some(plan) = plan {
            let mut previous = self.previous_stable_prefix.lock().await;
            let reused = previous
                .map(|fingerprint| fingerprint == plan.stable_prefix_fingerprint)
                .unwrap_or(false);
            *previous = Some(plan.stable_prefix_fingerprint);
            self.audits
                .lock()
                .await
                .push(plan.finalize(&response, reused));
        }

        Ok(CompletionStream::from_response(response))
    }
}

fn is_query_rewrite_request(request: &CompletionRequest) -> bool {
    request.tools.is_empty()
        && request.messages.len() == 2
        && request.messages[0]
            .content
            .starts_with("You are a query rewriter for an AI agent system.")
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, MOA_ANTHROPIC_API_KEY, and performs live cache audits"]
async fn live_cache_audit_tracks_same_session_cross_session_and_model_switch() -> Result<()> {
    if !live_provider_tests_enabled() {
        return Ok(());
    }
    require_live_env("MOA_ANTHROPIC_API_KEY", "Anthropic cache audit");

    let repo_root = repo_root()?;

    let storage_partition_id = StoragePartitionId::new("cache-audit");
    let user_id = UserId::new("cache-audit-user");

    let mut sonnet_config = live_cache_config("anthropic", "claude-sonnet-4-6", &repo_root);
    sonnet_config.providers.anthropic.api_key =
        env::var("MOA_ANTHROPIC_API_KEY").expect("require_live_env checked MOA_ANTHROPIC_API_KEY");

    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    let store = Arc::new(store);
    let tool_router = Arc::new(
        ToolRouter::from_config(&sonnet_config, None)
            .await?
            .with_rule_store(store.clone())
            .with_session_store(store.clone()),
    );
    tool_router
        .remember_workspace_root(
            tenant_id_from_storage_partition_id(&storage_partition_id),
            repo_root.clone(),
        )
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
    let sonnet_pipeline =
        build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
            &sonnet_config,
            store.clone(),
            GraphMemoryPipelineOptions {
                graph_pool: store.pool().clone(),
                kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
                shared_graph_memory_retriever: None,
                retrieval_embedder: None,
                shared_skill_injector: None,
                segment_store: Some(store.clone()),
                compaction_llm_provider: Some(sonnet_provider.clone()),
                query_rewrite_llm_provider: Some(sonnet_provider.clone()),
                identity_prompt_override: None,
                tool_schemas: tool_router.tool_schemas(),
                lineage: Arc::new(moa_core::traits::NullLineageHandle),
            },
        );

    let session_a = create_session(
        store.clone(),
        &storage_partition_id,
        &user_id,
        "claude-sonnet-4-6",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
        session_a,
        sonnet_provider.clone(),
        &sonnet_pipeline,
        Some(tool_router.clone()),
        "Reply with READY and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
        session_a,
        sonnet_provider.clone(),
        &sonnet_pipeline,
        Some(tool_router.clone()),
        "Reply with STEADY and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
        session_a,
        sonnet_provider.clone(),
        &sonnet_pipeline,
        Some(tool_router.clone()),
        "In this repository, what is the package name in crates/moa-brain/Cargo.toml? Use tools if needed and answer with just the value.",
    )
    .await?;

    let cross_session_audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let cross_session_provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
        build_provider_from_config(&sonnet_config)?,
        "cross_session_sonnet",
        vec!["fresh_session_repeat".to_string()],
        cross_session_audits.clone(),
    ));
    let cross_session_pipeline =
        build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
            &sonnet_config,
            store.clone(),
            GraphMemoryPipelineOptions {
                graph_pool: store.pool().clone(),
                kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
                shared_graph_memory_retriever: None,
                retrieval_embedder: None,
                shared_skill_injector: None,
                segment_store: Some(store.clone()),
                compaction_llm_provider: Some(cross_session_provider.clone()),
                query_rewrite_llm_provider: Some(cross_session_provider.clone()),
                identity_prompt_override: None,
                tool_schemas: tool_router.tool_schemas(),
                lineage: Arc::new(moa_core::traits::NullLineageHandle),
            },
        );
    let session_b = create_session(
        store.clone(),
        &storage_partition_id,
        &user_id,
        "claude-sonnet-4-6",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
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
    let mut cold_config = sonnet_config.clone();
    cold_config.general.workspace_instructions = Some(salted_workspace_instructions(
        cold_config.general.workspace_instructions.as_deref(),
        &format!("cache-audit-salt:{}", Uuid::now_v7()),
    ));
    let cold_session_pipeline =
        build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
            &cold_config,
            store.clone(),
            GraphMemoryPipelineOptions {
                graph_pool: store.pool().clone(),
                kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
                shared_graph_memory_retriever: None,
                retrieval_embedder: None,
                shared_skill_injector: None,
                segment_store: Some(store.clone()),
                compaction_llm_provider: Some(cold_session_provider.clone()),
                query_rewrite_llm_provider: Some(cold_session_provider.clone()),
                identity_prompt_override: None,
                tool_schemas: tool_router.tool_schemas(),
                lineage: Arc::new(moa_core::traits::NullLineageHandle),
            },
        );
    let session_c = create_session(
        store.clone(),
        &storage_partition_id,
        &user_id,
        "claude-sonnet-4-6",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
        session_c,
        cold_session_provider.clone(),
        &cold_session_pipeline,
        Some(tool_router.clone()),
        "Reply with COLD and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
        session_c,
        cold_session_provider.clone(),
        &cold_session_pipeline,
        Some(tool_router.clone()),
        "Reply with WARM and nothing else.",
    )
    .await?;

    let mut opus_config = sonnet_config.clone();
    opus_config.models.main = "claude-opus-4-6".to_string();
    let model_switch_audits = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let opus_provider: Arc<dyn LLMProvider> = Arc::new(AuditedProvider::new(
        build_provider_from_config(&opus_config)?,
        "model_switch_opus",
        vec!["switch_cold".to_string(), "switch_warm".to_string()],
        model_switch_audits.clone(),
    ));
    let opus_pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &opus_config,
        store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: store.pool().clone(),
            kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
            shared_graph_memory_retriever: None,
            retrieval_embedder: None,
            shared_skill_injector: None,
            segment_store: Some(store.clone()),
            compaction_llm_provider: Some(opus_provider.clone()),
            query_rewrite_llm_provider: Some(opus_provider.clone()),
            identity_prompt_override: None,
            tool_schemas: tool_router.tool_schemas(),
            lineage: Arc::new(moa_core::traits::NullLineageHandle),
        },
    );
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
        session_a,
        opus_provider.clone(),
        &opus_pipeline,
        Some(tool_router.clone()),
        "Reply with SWITCHED and nothing else.",
    )
    .await?;
    run_turn(
        store.clone(),
        test_identity(tenant_id_from_storage_partition_id(&storage_partition_id)),
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
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    model: &str,
) -> Result<moa_core::types::identifiers::SessionId> {
    store
        .create_session(session_meta(storage_partition_id, user_id, model))
        .await
}

fn session_meta(
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    model: &str,
) -> SessionMeta {
    let tenant_id = tenant_id_from_storage_partition_id(storage_partition_id);
    let contact_id = contact_id_from_user_id(user_id);
    SessionMeta {
        tenant_id,
        contact: Some(contact_ref(tenant_id, contact_id)),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model: moa_core::types::identifiers::ModelId::new(model),
        ..SessionMeta::default()
    }
}

fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    Uuid::parse_str(storage_partition_id.as_str())
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id.as_str())))
}

fn contact_id_from_user_id(user_id: &UserId) -> ContactId {
    Uuid::parse_str(user_id.as_str())
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id.as_str())))
}

use moa_test_support::fixtures::stable_uuid_from_label;

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}

async fn run_turn(
    store: Arc<dyn SessionStore>,
    identity: moa_core::traits::Identity,
    session_id: moa_core::types::identifiers::SessionId,
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

    let result = run_brain_turn(BrainTurnRequest {
        identity,
        session_id,
        session_store: store.clone(),
        llm_provider: provider,
        pipeline,
        tool_router,
    })
    .await?;

    assert_eq!(result, moa_brain::TurnResult::Complete);
    let _events = store.get_events(session_id, EventRange::all()).await?;
    Ok(())
}

fn test_identity(tenant_id: TenantId) -> moa_core::traits::Identity {
    moa_core::traits::Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c411),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    for candidate in cwd.ancestors() {
        if is_repo_root(candidate) {
            return Ok(candidate.to_path_buf());
        }
    }

    Err(moa_core::error::MoaError::ValidationError(format!(
        "could not locate repo root from {}",
        cwd.display()
    )))
}

fn is_repo_root(path: &Path) -> bool {
    path.join("Cargo.toml").exists() && path.join("crates/moa-brain").exists()
}

fn salted_workspace_instructions(base: Option<&str>, salt: &str) -> String {
    match base {
        Some(base) if !base.trim().is_empty() => {
            format!("{base}\nCache audit workspace instruction salt: {salt}")
        }
        _ => format!("Cache audit workspace instruction salt: {salt}"),
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
    if !live_provider_tests_enabled() {
        return Vec::new();
    }

    let mut configs = Vec::new();

    if let Ok(api_key) = env::var("MOA_ANTHROPIC_API_KEY")
        && !api_key.trim().is_empty()
    {
        let mut config = live_cache_config("anthropic", "claude-sonnet-4-6", repo_root);
        config.providers.anthropic.api_key = api_key;
        configs.push(("anthropic".to_string(), config));
    }
    if let Ok(api_key) = env::var("MOA_OPENAI_API_KEY")
        && !api_key.trim().is_empty()
    {
        let mut config = live_cache_config("openai", "gpt-5.4", repo_root);
        config.providers.openai.api_key = api_key;
        configs.push(("openai".to_string(), config));
    }
    if let Ok(api_key) = env::var("MOA_GOOGLE_API_KEY")
        && !api_key.trim().is_empty()
    {
        let mut config = live_cache_config("google", "gemini-3-flash-preview", repo_root);
        config.providers.google.api_key = api_key;
        configs.push(("google".to_string(), config));
    }
    assert!(
        !configs.is_empty(),
        "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires at least one provider credential for cache audits: MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY"
    );

    configs
}

fn live_provider_tests_enabled() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    env::var("MOA_RUN_LIVE_PROVIDER_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_live_env(name: &str, test_name: &str) {
    assert!(
        env::var(name).is_ok_and(|value| !value.trim().is_empty()),
        "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires {name} for {test_name}"
    );
}

fn live_cache_config(provider: &str, model: &str, repo_root: &Path) -> MoaConfig {
    let mut config = MoaConfig::default();
    config.general.default_provider = provider.to_string();
    config.models.main = model.to_string();
    config.general.workspace_instructions =
        Some("Cache audit static padding. Keep this prefix identical across turns.\n".repeat(220));
    config.local.sandbox_dir = repo_root.display().to_string();
    // This audit drives real read-only tool turns against the repo checkout,
    // which requires the development-only local hand provider.
    config
        .cloud
        .hands
        .get_or_insert_with(Default::default)
        .allow_local_provider = true;
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
        .messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count()
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
