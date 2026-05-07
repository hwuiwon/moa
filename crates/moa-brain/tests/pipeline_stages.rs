//! Per-stage context-pipeline contract tests.

mod support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use moa_brain::pipeline::cache::CacheOptimizer;
use moa_brain::pipeline::identity::IdentityProcessor;
use moa_brain::pipeline::instructions::InstructionProcessor;
use moa_brain::pipeline::memory::GraphMemoryRetriever;
use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_brain::pipeline::runtime_context::{Clock, RuntimeContextProcessor};
use moa_brain::pipeline::tools::ToolDefinitionProcessor;
use moa_core::{
    CacheBreakpoint, CacheBreakpointTarget, CacheTtl, CompletionContent, CompletionRequest,
    CompletionResponse, CompletionStream, ContextMessage, ContextProcessor, LLMProvider,
    MessageRole, ModelCapabilities, ModelId, QueryIntent, QueryRewriteConfig, QueryRewriteResult,
    Result, RewriteSource, StopReason, TokenUsage, WorkingContext, WorkspaceId,
};
use moa_memory_graph::{NodeLabel, PiiClass};
use moa_session::testing;
use serde_json::json;
use support::{MemoryHit, WorkingContextFixture, capabilities, mem_hit, tool_schema};

const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";
const HISTORY_END_INDEX_METADATA_KEY: &str = "_moa.history.end_index";
const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

#[tokio::test]
async fn identity_stage_emits_stable_system_message_with_workspace_and_runtime_metadata()
-> Result<()> {
    let mut fixture = WorkingContextFixture::new()
        .with_workspace_id("ws-001")
        .with_model_id("claude-sonnet-4-6")
        .with_messages(Vec::new())
        .build();

    // IdentityProcessor owns the stable system identity prompt and first cache breakpoint.
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
        fixture.ctx.cache_controls,
        vec![CacheBreakpoint::message(1, CacheTtl::OneHour)],
        "IdentityProcessor: should mark the identity message as a one-hour breakpoint"
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
        Some("Discovered AGENTS guidance.".to_string()),
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
        content.contains("Discovered AGENTS guidance."),
        "InstructionProcessor: missing discovered workspace instructions"
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
async fn runtime_stage_includes_cwd_and_now_and_workspace_id_in_runtime_block() -> Result<()> {
    let fixture = WorkingContextFixture::new()
        .with_workspace_id("ws-001")
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
        "<system-reminder>\nCurrent date: 2026-05-07\nCurrent workspace: ws-001\nCurrent working directory: {}\nCurrent user: user-007\n</system-reminder>",
        fixture.workspace_root.display()
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
            ContextMessage::user("fix the auth refresh jwt bug and add a regression test"),
        ])
        .build();
    let provider = Arc::new(RewriteProvider {
        response: json!({
            "rewritten_query": "fix the auth refresh jwt bug in auth.rs and add a regression test",
            "intent": "coding",
            "sub_queries": [
                "fix the auth refresh jwt bug in auth.rs",
                "add a regression test"
            ],
            "suggested_tools": ["file_read", "bash"],
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null
        })
        .to_string(),
        model_id: "rewrite-fixture".to_string(),
    });

    // QueryRewriter stores one structured rewrite result with preserved strategy sub-queries.
    let output = QueryRewriter::new(QueryRewriteConfig::default(), provider)
        .process(&mut fixture.ctx)
        .await?;

    assert_eq!(
        output.metadata.get("rewrite_source"),
        Some(&json!("rewritten")),
        "QueryRewriter: metadata should record rewritten source"
    );
    let result = rewrite_result(&fixture.ctx);
    assert_eq!(
        result.intent,
        QueryIntent::Coding,
        "QueryRewriter: intent changed"
    );
    assert_eq!(
        result.source,
        RewriteSource::Rewritten,
        "QueryRewriter: source changed"
    );
    assert_eq!(
        result.sub_queries,
        vec![
            "fix the auth refresh jwt bug in auth.rs".to_string(),
            "add a regression test".to_string()
        ],
        "QueryRewriter: strategy sub-query legs changed"
    );
    assert_eq!(
        result.suggested_tools,
        vec!["file_read".to_string(), "bash".to_string()],
        "QueryRewriter: suggested tool filtering changed"
    );
    Ok(())
}

#[tokio::test]
async fn memory_stage_includes_top_k_hits_with_lineage_uids_and_excludes_invalidated_nodes()
-> Result<()> {
    let fixture = WorkingContextFixture::new()
        .with_workspace_id("ws-001")
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
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    delete_memory_rows(store.pool(), &ctx.workspace_id).await?;
    seed_memory_rows(
        store.pool(),
        &ctx.workspace_id,
        &fixture.memory_hits,
        fixture.clock_at,
    )
    .await?;

    // GraphMemoryRetriever injects the top three active hits and excludes invalidated nodes.
    let output = GraphMemoryRetriever::new(store.pool().clone())
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
    delete_memory_rows(store.pool(), &ctx.workspace_id).await?;
    Ok(())
}

#[tokio::test]
async fn cache_stage_inserts_breakpoints_at_4_segment_boundaries() -> Result<()> {
    let mut ctx = WorkingContextFixture::new()
        .with_messages(Vec::new())
        .build()
        .ctx;
    ctx.append_system("identity");
    ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
    ctx.append_system("instructions");
    ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
    ctx.set_tools(vec![tool_schema("bash")]);
    ctx.extend_messages(vec![
        ContextMessage::assistant("previous reply"),
        ContextMessage::tool_result("toolu_1", "tool output", None),
        ContextMessage::user("current question"),
    ]);
    ctx.insert_metadata(HISTORY_END_INDEX_METADATA_KEY, json!(4));

    // CacheOptimizer preserves stable prefix boundaries and adds the conversation breakpoint.
    CacheOptimizer.process(&mut ctx).await?;

    assert_eq!(
        ctx.cache_controls,
        vec![
            CacheBreakpoint::tools(CacheTtl::OneHour),
            CacheBreakpoint::message(1, CacheTtl::OneHour),
            CacheBreakpoint::message(2, CacheTtl::OneHour),
            CacheBreakpoint::message(4, CacheTtl::FiveMinutes),
        ],
        "CacheOptimizer: planned cache-control boundaries changed"
    );
    assert_eq!(
        ctx.cache_breakpoints,
        vec![1, 2, 4],
        "CacheOptimizer: message-boundary cache breakpoints changed"
    );
    assert_eq!(
        ctx.cache_controls
            .iter()
            .map(cache_target_label)
            .collect::<Vec<_>>(),
        vec![
            "tools".to_string(),
            "message:1".to_string(),
            "message:2".to_string(),
            "message:4".to_string()
        ],
        "CacheOptimizer: cache-control targets changed"
    );
    Ok(())
}

#[test]
fn pipeline_stage_failure_message_names_the_stage_clearly() {
    for stage_name in [
        "IdentityProcessor",
        "InstructionProcessor",
        "ToolDefinitionProcessor",
        "RuntimeContextProcessor",
        "QueryRewriter",
        "GraphMemoryRetriever",
        "CacheOptimizer",
    ] {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            assert_stage_contract(stage_name, || {
                assert_eq!("actual", "expected", "deliberate contract mismatch");
            });
        }))
        .expect_err("stage contract harness should re-panic with the stage name");
        let message = panic_message(panic);
        assert!(
            message.contains(stage_name),
            "stage failure harness did not include {stage_name}: {message}"
        );
        assert!(
            message.contains("deliberate contract mismatch"),
            "stage failure harness dropped the original assertion message: {message}"
        );
    }
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
    workspace_id: &WorkspaceId,
    hits: &[MemoryHit],
    clock_at: DateTime<Utc>,
) -> Result<()> {
    for (index, hit) in hits.iter().enumerate() {
        let valid_to = (!hit.valid).then_some(clock_at);
        let last_accessed_at = clock_at + Duration::seconds(index as i64);
        sqlx::query(
            r#"
            INSERT INTO moa.node_index
                (uid, label, workspace_id, name, pii_class, confidence,
                 valid_to, properties_summary, last_accessed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(hit.uid)
        .bind(NodeLabel::Fact.as_str())
        .bind(workspace_id.as_str())
        .bind(&hit.name)
        .bind(PiiClass::None.as_str())
        .bind(0.99_f64)
        .bind(valid_to)
        .bind(json!({ "summary": hit.summary }))
        .bind(last_accessed_at)
        .execute(pool)
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    }
    Ok(())
}

async fn delete_memory_rows(pool: &sqlx::PgPool, workspace_id: &WorkspaceId) -> Result<()> {
    sqlx::query("DELETE FROM moa.node_index WHERE workspace_id = $1")
        .bind(workspace_id.as_str())
        .execute(pool)
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
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
        .ok_or_else(|| moa_core::MoaError::ValidationError("tool schema missing name".to_string()))
}

fn cache_target_label(breakpoint: &CacheBreakpoint) -> String {
    match breakpoint.target {
        CacheBreakpointTarget::ToolDefinitions => "tools".to_string(),
        CacheBreakpointTarget::MessageBoundary { index } => format!("message:{index}"),
    }
}

fn invalidated_uids_in_content(content: &str, hits: &[MemoryHit]) -> Vec<String> {
    hits.iter()
        .filter(|hit| !hit.valid)
        .map(|hit| hit.uid.to_string())
        .filter(|uid| content.contains(uid))
        .collect()
}

fn assert_stage_contract(stage_name: &str, assertion: impl FnOnce()) {
    match catch_unwind(AssertUnwindSafe(assertion)) {
        Ok(()) => {}
        Err(payload) => {
            let message = panic_message(payload);
            panic!("{stage_name}: {message}");
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    "non-string panic payload".to_string()
}
