//! Per-stage context-pipeline contract tests.

use std::collections::HashMap;
use std::sync::Arc;

use super::support::{
    MemoryHit, MockSessionStore, WorkingContextFixture, capabilities, mem_hit, session_meta,
    tool_schema,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use moa_brain::pipeline::identity::IdentityProcessor;
use moa_brain::pipeline::instructions::InstructionProcessor;
use moa_brain::pipeline::memory::GraphMemoryRetriever;
use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_brain::pipeline::runtime_context::{Clock, RuntimeContextProcessor};
use moa_brain::pipeline::tools::ToolDefinitionProcessor;
use moa_brain::query_rewrite::{QueryRewriteResult, RewriteReason, RewriteSource};
use moa_brain::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
};
use moa_config::MoaConfig;
use moa_config::QueryRewriteConfig;
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    error::Result, traits::ContextProcessor, traits::Identity, traits::IdentityType,
    traits::LLMProvider, traits::NullLineageHandle, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::CompletionStream, types::completion::StopReason,
    types::completion::TokenUsage, types::contact::ContactId, types::context::ContextMessage,
    types::context::MessageRole, types::context::TURN_ID_METADATA_KEY,
    types::context::WorkingContext, types::identifiers::ModelId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::model::ModelCapabilities, types::observability::stable_prefix_fingerprint,
};
use moa_crypto::LocalKmsProvider;
use moa_db::ScopedConn;
use moa_memory_graph::NodeLabel;
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION, VectorItem, VectorStore};
use moa_session::testing;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";
const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

#[tokio::test]
async fn digest_processor_registers_at_documented_position() {
    // Pins: history compiles before the per-turn dynamic sections (skill
    // manifest, standing digest, graph memory) so those sections insert near
    // the active user turn instead of ahead of replayed history — per-turn
    // churn there would break provider prompt-cache reuse of the whole
    // history span. History compilation remains the only compaction owner.
    let mut config = MoaConfig::default();
    config.memory.digest.enabled = true;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/moa_test")
        .expect("lazy pool should not connect");
    let session_store = Arc::new(MockSessionStore::new(
        session_meta("digest-pipeline", "mock"),
        Vec::new(),
    ));

    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        session_store,
        GraphMemoryPipelineOptions {
            graph_pool: pool,
            kms: Arc::new(LocalKmsProvider::new()),
            shared_graph_memory_retriever: None,
            retrieval_embedder: None,
            shared_skill_injector: None,
            segment_store: None,
            compaction_llm_provider: None,
            query_rewrite_llm_provider: None,
            identity_prompt_override: None,
            tool_schemas: Vec::new(),
            lineage: Arc::new(NullLineageHandle),
        },
    );
    let names = pipeline.stage_names();
    let digest = names
        .iter()
        .position(|name| *name == "memory_digest")
        .expect("digest processor should be registered");
    let graph_memory = names
        .iter()
        .position(|name| *name == "graph_memory")
        .expect("graph memory processor should be registered");
    let history = names
        .iter()
        .position(|name| *name == "history")
        .expect("history processor should be registered");

    assert!(history < digest);
    assert!(digest < graph_memory);
    assert_eq!(names[digest + 1], "graph_memory");
    assert!(
        !names.contains(&"compactor"),
        "history owns checkpoint compaction; stage-10 compactor must stay removed"
    );
}

#[tokio::test]
async fn identity_stage_emits_stable_system_message_with_workspace_and_runtime_metadata()
-> Result<()> {
    let mut fixture = WorkingContextFixture::new()
        .with_storage_partition_id("ws-001")
        .with_model_id("claude-sonnet-4-6")
        .with_messages(Vec::new())
        .build();

    // Pins: IdentityProcessor owns the first stable system identity prompt section.
    let output = IdentityProcessor::default()
        .process(&mut fixture.ctx)
        .await?;

    assert_eq!(
        IdentityProcessor::default().name(),
        "identity",
        "IdentityProcessor: stage name changed"
    );
    assert_eq!(
        IdentityProcessor::default().stage(),
        1,
        "IdentityProcessor: stage number changed"
    );
    assert_eq!(
        fixture.ctx.messages.len(),
        1,
        "IdentityProcessor: expected exactly one system message"
    );
    assert_eq!(
        fixture.ctx.messages[0].role,
        MessageRole::System,
        "IdentityProcessor: message role should be system"
    );
    insta::assert_snapshot!(
        "identity_stage_system_message",
        fixture.ctx.messages[0].content
    );
    assert!(
        fixture.ctx.tools().is_empty(),
        "IdentityProcessor: should not add tool definitions"
    );
    assert_eq!(
        fixture.ctx.metadata().len(),
        1,
        "IdentityProcessor: should not mutate existing runtime metadata"
    );
    assert_eq!(
        output.items_included,
        vec!["moa_identity".to_string()],
        "IdentityProcessor: included item id changed"
    );
    Ok(())
}

#[tokio::test]
async fn instruction_stage_appends_workspace_instructions_when_present_and_skips_when_absent()
-> Result<()> {
    let mut with_instructions = WorkingContextFixture::new()
        .with_messages(Vec::new())
        .build()
        .ctx;

    // InstructionProcessor emits configured workspace and user guidance, and is a no-op when empty.
    let output = InstructionProcessor::new(
        Some("Follow repo conventions.".to_string()),
        Some("Keep responses terse.".to_string()),
    )
    .process(&mut with_instructions)
    .await?;

    assert_eq!(
        with_instructions.messages.len(),
        1,
        "InstructionProcessor: expected one instruction message"
    );
    let content = &with_instructions.messages[0].content;
    assert_eq!(
        with_instructions.messages[0].role,
        MessageRole::System,
        "InstructionProcessor: instructions should be system content"
    );
    assert!(
        content.contains("<workspace_instructions>\nFollow repo conventions."),
        "InstructionProcessor: missing workspace instruction block"
    );
    assert!(
        content.contains("<user_preferences>\nKeep responses terse."),
        "InstructionProcessor: missing user preferences block"
    );
    assert_eq!(
        output.items_included,
        vec![
            "workspace_instructions".to_string(),
            "user_instructions".to_string()
        ],
        "InstructionProcessor: included item ids changed"
    );

    let mut without_instructions = WorkingContextFixture::new()
        .with_messages(Vec::new())
        .build()
        .ctx;
    let empty_output = InstructionProcessor::default()
        .process(&mut without_instructions)
        .await?;
    assert_eq!(
        without_instructions.messages,
        Vec::<ContextMessage>::new(),
        "InstructionProcessor: empty instructions should not add messages"
    );
    assert_eq!(
        empty_output.items_included,
        Vec::<String>::new(),
        "InstructionProcessor: empty instructions should not report included items"
    );
    Ok(())
}

#[tokio::test]
async fn tool_definition_stage_caps_tool_count_at_max_and_orders_deterministically() -> Result<()> {
    let tool_names = (0..50)
        .rev()
        .map(|index| format!("tool_{index:02}"))
        .collect::<Vec<_>>();
    let tool_refs = tool_names.iter().map(String::as_str).collect::<Vec<_>>();
    let fixture = WorkingContextFixture::new()
        .with_tools(&tool_refs)
        .with_messages(Vec::new())
        .build();
    let mut ctx = fixture.ctx;

    // ToolDefinitionProcessor sorts schemas by name and caps the stable prefix at 30 tools.
    let output = ToolDefinitionProcessor::new(fixture.tool_schemas.clone())
        .process(&mut ctx)
        .await?;

    let expected_names = (0..30)
        .map(|index| format!("tool_{index:02}"))
        .collect::<Vec<_>>();
    let actual_names = ctx
        .tools()
        .iter()
        .map(tool_name)
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        actual_names, expected_names,
        "ToolDefinitionProcessor: tool ordering or cap changed"
    );
    assert_eq!(
        output.items_included, expected_names,
        "ToolDefinitionProcessor: reported included tools should match output schemas"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_stage_includes_cwd_and_now_and_storage_partition_id_in_runtime_block() -> Result<()>
{
    let fixture = WorkingContextFixture::new()
        .with_storage_partition_id("ws-001")
        .with_user_id("user-007")
        .with_clock_at("2026-05-07T12:00:00Z")
        .with_messages(vec![
            ContextMessage::assistant("Earlier answer"),
            ContextMessage::user("Current turn prompt"),
        ])
        .build();
    let mut ctx = fixture.ctx;

    // RuntimeContextProcessor inserts volatile runtime facts immediately before the active user turn.
    RuntimeContextProcessor::new(Arc::new(FixedClock::new(fixture.clock_at)))
        .process(&mut ctx)
        .await?;

    assert_eq!(
        ctx.messages.len(),
        3,
        "RuntimeContextProcessor: expected one runtime reminder insertion"
    );
    assert_eq!(
        ctx.messages[1].role,
        MessageRole::User,
        "RuntimeContextProcessor: runtime reminder should be user-role system-reminder content"
    );
    assert_eq!(
        ctx.messages[2].content, "Current turn prompt",
        "RuntimeContextProcessor: active user turn should remain last"
    );
    let expected = format!(
        "<system-reminder>\nCurrent date: 2026-05-07\nCurrent project: ws-001\nCurrent working directory: {}\nCurrent tenant: {}\nCurrent contact: {}\n</system-reminder>",
        fixture.workspace_root.display(),
        ctx.tenant_id,
        ctx.contact
            .as_ref()
            .expect("runtime fixture should have a contact")
            .contact_id
    );
    assert_eq!(
        ctx.messages[1].content, expected,
        "RuntimeContextProcessor: runtime reminder content changed"
    );
    Ok(())
}

#[tokio::test]
async fn query_rewrite_stage_emits_one_leg_per_strategy_for_a_user_query() -> Result<()> {
    let mut fixture = WorkingContextFixture::new()
        .with_tools(&["bash", "file_read"])
        .with_messages(vec![
            ContextMessage::user("The auth refresh jwt bug is in auth.rs"),
            ContextMessage::assistant("I found the auth.rs refresh path."),
            ContextMessage::user("fix that and add a regression test"),
        ])
        .build();
    let provider = Arc::new(RewriteProvider {
        response: json!({
            "retrieval_query": "fix the auth refresh jwt bug in auth.rs and add a regression test",
            "is_new_task": false,
            "task_summary": null
        })
        .to_string(),
        model_id: "rewrite-fixture".to_string(),
    });

    // QueryRewriter stores one retrieval-focused rewrite result.
    let output = QueryRewriter::new(QueryRewriteConfig::default(), provider)
        .with_retrieval_availability(true, true)
        .process(&mut fixture.ctx)
        .await?;

    assert_eq!(
        output.metadata.get("rewrite_source"),
        Some(&json!("rewrite")),
        "QueryRewriter: metadata should record rewrite decision"
    );
    let result = rewrite_result(&fixture.ctx);
    assert_eq!(
        result.source,
        RewriteSource::Rewritten,
        "QueryRewriter: source changed"
    );
    assert_eq!(
        result.retrieval_query, "fix the auth refresh jwt bug in auth.rs and add a regression test",
        "QueryRewriter: retrieval query changed"
    );
    assert_eq!(result.reason, Some(RewriteReason::CoreferenceWithHistory));
    Ok(())
}

#[tokio::test]
async fn memory_stage_includes_top_k_hits_with_lineage_uids_and_excludes_invalidated_nodes()
-> Result<()> {
    let fixture = WorkingContextFixture::new()
        .with_storage_partition_id("ws-001")
        .with_user_message("auth jwt memory")
        .with_memory_hits(&[
            mem_hit("auth jwt memory valid 00", "uses jwt zero"),
            mem_hit("auth jwt memory valid 01", "uses jwt one"),
            mem_hit("auth jwt memory valid 02", "uses jwt two"),
            mem_hit("auth jwt memory valid 03", "uses jwt three"),
            mem_hit("auth jwt memory valid 04", "uses jwt four"),
            mem_hit("auth jwt memory invalid 05", "invalid jwt five").invalidated(),
            mem_hit("auth jwt memory invalid 06", "invalid jwt six").invalidated(),
            mem_hit("auth jwt memory invalid 07", "invalid jwt seven").invalidated(),
            mem_hit("auth jwt memory invalid 08", "invalid jwt eight").invalidated(),
            mem_hit("auth jwt memory invalid 09", "invalid jwt nine").invalidated(),
        ])
        .build();
    let mut ctx = fixture.ctx;
    let runtime_tenant_id = ctx.tenant_id;
    let runtime_contact_id = ctx
        .contact
        .as_ref()
        .expect("memory-stage fixture should have a contact")
        .contact_id;
    ctx.set_caller_identity(Identity {
        identity_type: IdentityType::Contact,
        id: runtime_contact_id.0,
        tenant_id: runtime_tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    });
    ctx.insert_metadata(TURN_ID_METADATA_KEY, json!(Uuid::now_v7().to_string()));
    let runtime_storage_partition_id = StoragePartitionId::for_tenant(runtime_tenant_id);
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    delete_memory_rows(store.pool(), &runtime_storage_partition_id).await?;
    seed_memory_rows(
        store.pool(),
        runtime_tenant_id,
        runtime_contact_id,
        &runtime_storage_partition_id,
        &fixture.memory_hits,
        fixture.clock_at,
    )
    .await?;
    let vector_noise_uid = seed_other_tenant_vector_noise(store.pool(), fixture.clock_at).await?;

    // GraphMemoryRetriever injects the top three active lexical hits, excludes invalidated nodes,
    // and does not retrieve vector-only rows from another tenant when no real query embedding exists.
    //
    // Whole-window abstention is disabled for this stage: its 0.68 threshold is
    // calibrated for the production embed-v4.0 cosine floor, but this test drives
    // the lexical admission path with no embedder, so evidence is lexical-only and
    // cannot reach a cosine-scaled threshold. Abstention itself is pinned directly
    // in the retrieval::hybrid unit tests; here the invariant under test is top-k
    // admission plus invalidated-node exclusion.
    let output = GraphMemoryRetriever::new_with_config(
        abstention_disabled_config(),
        store.pool().clone(),
        Arc::new(LocalKmsProvider::new()),
        None,
    )
    .with_assume_app_role(true)
    .with_result_limit(3)
    .process(&mut ctx)
    .await?;

    let expected_hits = fixture.memory_hits[2..5].iter().rev().collect::<Vec<_>>();
    let expected_items = expected_hits
        .iter()
        .map(|hit| format!("graph:Fact:{}", hit.uid))
        .collect::<Vec<_>>();
    assert_eq!(
        output.items_included, expected_items,
        "GraphMemoryRetriever: included memory item ids changed"
    );
    let memory_message = ctx
        .messages
        .first()
        .expect("GraphMemoryRetriever: memory reminder should be inserted before user turn");
    assert_eq!(
        memory_message.role,
        MessageRole::User,
        "GraphMemoryRetriever: memory reminder should be user-role content"
    );
    assert!(
        memory_message.content.starts_with(MEMORY_REMINDER_PREFIX),
        "GraphMemoryRetriever: memory reminder prefix changed"
    );
    assert!(
        memory_message
            .content
            .contains("Use these hits as background evidence"),
        "GraphMemoryRetriever: memory reminder should frame retrieved memory as evidence"
    );
    assert!(
        memory_message.content.contains("scope=")
            && memory_message.content.contains("valid_from=")
            && memory_message.content.contains("legs="),
        "GraphMemoryRetriever: memory reminder should expose provenance and age fields"
    );
    for hit in expected_hits {
        assert!(
            memory_message.content.contains(&hit.uid.to_string()),
            "GraphMemoryRetriever: missing lineage uid {}",
            hit.uid
        );
        assert!(
            memory_message.content.contains(&hit.summary),
            "GraphMemoryRetriever: missing summary for {}",
            hit.uid
        );
    }
    assert_eq!(
        invalidated_uids_in_content(&memory_message.content, &fixture.memory_hits),
        Vec::<String>::new(),
        "GraphMemoryRetriever: invalidated memory nodes leaked into prompt"
    );
    assert!(
        !output
            .items_included
            .contains(&format!("graph:Fact:{vector_noise_uid}")),
        "GraphMemoryRetriever: vector-only cross-tenant noise leaked without a query embedding"
    );
    delete_other_tenant_vector_noise(store.pool(), vector_noise_uid).await?;
    delete_memory_rows(store.pool(), &runtime_storage_partition_id).await?;
    Ok(())
}

#[test]
fn stable_prefix_fingerprint_uses_tools_and_leading_system_sections_only() {
    // Pins: provider cache keys stay stable when only dynamic tail messages change.
    let tools = vec![tool_schema("bash")];
    let base_messages = vec![
        ContextMessage::system("identity"),
        ContextMessage::system("instructions"),
    ];
    let first = CompletionRequest {
        model: Some(ModelId::new("claude-sonnet-4-6")),
        messages: base_messages
            .iter()
            .cloned()
            .chain([
                ContextMessage::assistant("previous reply"),
                ContextMessage::tool_result("toolu_1", "tool output", None),
                ContextMessage::user("current question"),
            ])
            .collect(),
        tools: tools.clone(),
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    };
    let second = CompletionRequest {
        model: Some(ModelId::new("claude-sonnet-4-6")),
        messages: base_messages
            .into_iter()
            .chain([
                ContextMessage::assistant("different previous reply"),
                ContextMessage::tool_result("toolu_1", "different tool output", None),
                ContextMessage::user("different current question"),
            ])
            .collect(),
        tools,
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    };

    assert_eq!(
        stable_prefix_fingerprint(&first),
        stable_prefix_fingerprint(&second),
        "stable prefix should ignore non-system dynamic tail messages"
    );
}

#[derive(Debug)]
struct FixedClock {
    now: DateTime<Utc>,
}

impl FixedClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self { now }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
}

#[derive(Debug)]
struct RewriteProvider {
    response: String,
    model_id: String,
}

#[async_trait]
impl LLMProvider for RewriteProvider {
    fn name(&self) -> &str {
        "rewrite-fixture"
    }

    fn capabilities(&self) -> ModelCapabilities {
        capabilities(&self.model_id)
    }

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
        let response = CompletionResponse {
            text: self.response.clone(),
            content: vec![CompletionContent::Text(self.response.clone())],
            stop_reason: StopReason::EndTurn,
            model: ModelId::new(self.model_id.clone()),
            usage: TokenUsage::default(),
            duration_ms: 1,
            thought_signature: None,
        };
        Ok(CompletionStream::from_response(response))
    }
}

async fn seed_memory_rows(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    storage_partition_id: &StoragePartitionId,
    hits: &[MemoryHit],
    clock_at: DateTime<Utc>,
) -> Result<()> {
    let mut conn = ScopedConn::begin(pool, &RlsContext::contact(tenant_id, contact_id))
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    for (index, hit) in hits.iter().enumerate() {
        let valid_to = (!hit.valid).then_some(clock_at);
        let last_accessed_at = clock_at + Duration::seconds(index as i64);
        sqlx::query(
            r#"
            INSERT INTO moa.node_index
                (uid, label, storage_partition_id, user_id, data_subject_id, name, pii_class,
                 confidence, valid_to, properties_summary, last_accessed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(hit.uid)
        .bind(NodeLabel::Fact.as_str())
        .bind(storage_partition_id.as_str())
        .bind(contact_id.to_string())
        .bind(contact_id.0)
        .bind(&hit.name)
        .bind(SensitivityClass::None.as_str())
        .bind(0.99_f64)
        .bind(valid_to)
        .bind(json!({ "summary": hit.summary }))
        .bind(last_accessed_at)
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    }
    conn.commit()
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

async fn seed_other_tenant_vector_noise(
    pool: &sqlx::PgPool,
    clock_at: DateTime<Utc>,
) -> Result<Uuid> {
    let uid = Uuid::from_u128(0x2_000);
    let other_tenant_id = TenantId::new();
    let other_storage_partition_id = StoragePartitionId::for_tenant(other_tenant_id).to_string();
    seed_workspace_embedder_state(
        pool,
        &other_tenant_id,
        &other_storage_partition_id,
        "pipeline-stage-test",
    )
    .await?;
    sqlx::query("DELETE FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .execute(pool)
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, user_id, data_subject_id, name, pii_class,
             confidence, properties_summary, last_accessed_at)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(uid)
    .bind(NodeLabel::Fact.as_str())
    .bind(&other_storage_partition_id)
    .bind(other_tenant_id.0)
    .bind("unrelated cross-tenant vector noise")
    .bind(SensitivityClass::None.as_str())
    .bind(0.99_f64)
    .bind(json!({ "summary": "unrelated cross-tenant vector noise" }))
    .bind(clock_at + Duration::seconds(10_000))
    .execute(pool)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;

    let vector = PgvectorStore::new(pool.clone(), RlsContext::tenant(other_tenant_id));
    vector
        .upsert(&[VectorItem {
            uid,
            user_id: None,
            label: NodeLabel::Fact.as_str().to_string(),
            pii_class: SensitivityClass::None,
            embedding: vec![0.0; VECTOR_DIMENSION],
            embedding_model: "pipeline-stage-test".to_string(),
            embedding_model_version: 1,
            search_text: None,
            valid_to: None,
        }])
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;

    Ok(uid)
}

async fn seed_workspace_embedder_state(
    pool: &sqlx::PgPool,
    tenant_id: &TenantId,
    storage_partition_id: &str,
    model: &str,
) -> Result<()> {
    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(*tenant_id))
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, 1, $3)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(storage_partition_id)
    .bind(model)
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit()
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

async fn delete_memory_rows(
    pool: &sqlx::PgPool,
    storage_partition_id: &StoragePartitionId,
) -> Result<()> {
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = $1")
        .bind(storage_partition_id.as_str())
        .execute(pool)
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

async fn delete_other_tenant_vector_noise(pool: &sqlx::PgPool, uid: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .execute(pool)
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

fn rewrite_result(ctx: &WorkingContext) -> QueryRewriteResult {
    serde_json::from_value(
        ctx.metadata()
            .get(QUERY_REWRITE_METADATA_KEY)
            .expect("QueryRewriter: query_rewrite metadata should exist")
            .clone(),
    )
    .expect("QueryRewriter: query_rewrite metadata should deserialize")
}

fn tool_name(schema: &serde_json::Value) -> Result<String> {
    schema
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            moa_core::error::MoaError::ValidationError("tool schema missing name".to_string())
        })
}

fn invalidated_uids_in_content(content: &str, hits: &[MemoryHit]) -> Vec<String> {
    hits.iter()
        .filter(|hit| !hit.valid)
        .map(|hit| hit.uid.to_string())
        .filter(|uid| content.contains(uid))
        .collect()
}

/// Runtime config with memory-stage whole-window abstention disabled.
///
/// The production `abstain_below_window_evidence` default (0.68) is calibrated
/// for the embed-v4.0 cosine floor. Hermetic pipeline tests drive retrieval with
/// a mock or absent embedder, where per-hit evidence is lexical-only and cannot
/// reach a cosine-scaled threshold, so leaving abstention on would clear every
/// hit regardless of admission. Abstention is pinned directly in the
/// `moa_retrieval::retrieval::hybrid` unit tests; disabling it here keeps these
/// tests focused on admission, scope boundaries, and lineage.
fn abstention_disabled_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config
        .memory
        .retrieval
        .ranking
        .abstain_below_window_evidence = 0.0;
    config
}
