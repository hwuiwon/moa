//! Offline coverage for the agentic memory tools (plan Task 11): the harness
//! only surfaces `memory_search`/`memory_navigate` when the memory stage gated
//! them on, and when offered they execute through the retrieval executor and
//! return per-hit provenance.

#[path = "support/offline_session_store.rs"]
mod offline_session_store;

use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::pipeline::ContextPipeline;
use moa_brain::pipeline::memory::OFFER_RETRIEVAL_TOOLS_METADATA_KEY;
use moa_brain::{TurnResult, run_brain_turn};
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, ContextProcessor,
    Event, EventRange, LLMProvider, MemoryRetrievalExecutor, MoaConfig, ModelCapabilities, ModelId,
    ProcessorOutput, Result, SessionMeta, SessionStore, StopReason, TokenPricing, TokenUsage,
    ToolCallContent, ToolCallFormat, ToolInvocation, ToolOutput, WorkingContext,
};
use moa_hands::ToolRouter;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::sync::Mutex;
use uuid::Uuid;

use offline_session_store::{MockSessionStore, session_meta};

fn usage(input: usize, output: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: output,
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("claude-sonnet-4-6"),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

/// Test stage that pins the agentic-memory-tool gate decision the real memory
/// stage would compute, so harness gating can be exercised without Postgres.
struct GateProcessor {
    offer: bool,
}

#[async_trait]
impl ContextProcessor for GateProcessor {
    fn name(&self) -> &str {
        "test_gate"
    }

    fn stage(&self) -> u8 {
        9
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        ctx.insert_metadata(OFFER_RETRIEVAL_TOOLS_METADATA_KEY, Value::Bool(self.offer));
        Ok(ProcessorOutput::default())
    }
}

/// Canned graph/chunk uids the executor returns as provenance.
const GRAPH_UID: Uuid = Uuid::from_u128(0xA1);
const CHUNK_UID: Uuid = Uuid::from_u128(0xA2);
const DOCUMENT_VERSION_UID: Uuid = Uuid::from_u128(0xA3);

/// Retrieval executor stand-in returning one hit with full provenance so the
/// tool-output contract can be asserted without the real retrieval stack.
struct TestRetrievalExecutor;

#[async_trait]
impl MemoryRetrievalExecutor for TestRetrievalExecutor {
    async fn execute_retrieval_tool(
        &self,
        _session: &SessionMeta,
        tool_name: &str,
        _input: &Value,
    ) -> Result<ToolOutput> {
        assert_eq!(tool_name, "memory_search");
        Ok(ToolOutput::json(
            "1 memory hit".to_string(),
            json!({
                "hits": [{
                    "graph_uid": GRAPH_UID,
                    "label": "Chunk",
                    "title": "Rotation policy",
                    "excerpt": "API keys rotate every 90 days.",
                    "score": 0.91,
                    "chunk_uid": CHUNK_UID,
                    "document_version_uid": DOCUMENT_VERSION_UID,
                    "source_uri": "https://kb.example.invalid/rotation"
                }]
            }),
            std::time::Duration::from_millis(1),
        ))
    }
}

/// Provider that captures every request and always ends the turn with text, so
/// the compiled tool loadout can be inspected.
#[derive(Clone)]
struct CapturingProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl CapturingProvider {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl LLMProvider for CapturingProvider {
    fn name(&self) -> &str {
        "capturing"
    }

    fn capabilities(&self) -> ModelCapabilities {
        capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        self.requests.lock().await.push(request);
        Ok(CompletionStream::from_response(CompletionResponse {
            text: "done".to_string(),
            content: vec![CompletionContent::Text("done".to_string())],
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("claude-sonnet-4-6"),
            usage: usage(20, 4),
            duration_ms: 5,
            thought_signature: None,
        }))
    }
}

/// Provider that calls `memory_search` on its first turn, then ends. On the
/// second request it asserts the tool result carried the hit provenance.
#[derive(Default)]
struct MemorySearchThenEndProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for MemorySearchThenEndProvider {
    fn name(&self) -> &str {
        "memory-search-loop"
    }

    fn capabilities(&self) -> ModelCapabilities {
        capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("55555555-5555-5555-5555-555555555555".to_string()),
                        name: "memory_search".to_string(),
                        input: json!({ "query": "api key rotation policy" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: ModelId::new("claude-sonnet-4-6"),
                usage: usage(12, 5),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            let tool_message = request
                .messages
                .iter()
                .find(|message| message.role == moa_core::MessageRole::Tool)
                .expect("memory_search tool result must be replayed into context");
            let graph_uid = GRAPH_UID.to_string();
            let chunk_uid = CHUNK_UID.to_string();
            for needle in [
                graph_uid.as_str(),
                chunk_uid.as_str(),
                "https://kb.example.invalid/rotation",
            ] {
                assert!(
                    tool_message.content.contains(needle),
                    "tool result must carry provenance `{needle}`; got: {}",
                    tool_message.content
                );
            }
            CompletionResponse {
                text: "Keys rotate every 90 days.".to_string(),
                content: vec![CompletionContent::Text(
                    "Keys rotate every 90 days.".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("claude-sonnet-4-6"),
                usage: usage(20, 7),
                duration_ms: 12,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

fn pipeline_with_gate(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    tool_schemas: Vec<Value>,
    offer: bool,
) -> ContextPipeline {
    let stages: Vec<Box<dyn ContextProcessor>> = vec![
        Box::new(moa_brain::pipeline::identity::IdentityProcessor::default()),
        Box::new(moa_brain::pipeline::tools::ToolDefinitionProcessor::new(
            tool_schemas,
        )),
        Box::new(
            moa_brain::pipeline::history::HistoryCompiler::new(session_store)
                .with_compaction_config(config.compaction.clone())
                .with_tool_output_config(config.tool_output.clone())
                .with_snapshot_config(config.context_snapshot.clone()),
        ),
        Box::new(GateProcessor { offer }),
    ];
    ContextPipeline::with_runtime_limits(
        stages,
        config.budgets.daily_tenant_cents,
        config.context_snapshot.clone(),
    )
}

fn request_tool_names(request: &CompletionRequest) -> Vec<String> {
    request
        .tools
        .iter()
        .filter_map(|schema| schema.get("name").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect()
}

async fn run_capturing_turn(offer: bool) -> Vec<String> {
    let config = MoaConfig::default();
    let session = session_meta("agentic-memory-gate", "claude-sonnet-4-6");
    let session_id = session.id;
    let store = Arc::new(MockSessionStore::new(session, Vec::new()));
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "What is my API key rotation policy?".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let sandbox_dir = tempdir().unwrap();
    let tool_router = Arc::new(
        ToolRouter::new_local(sandbox_dir.path())
            .await
            .unwrap()
            .with_memory_retrieval_executor(Arc::new(TestRetrievalExecutor)),
    );
    let pipeline = pipeline_with_gate(&config, store.clone(), tool_router.tool_schemas(), offer);
    let provider = Arc::new(CapturingProvider::new());

    let result = run_brain_turn(
        session_id,
        store.clone(),
        provider.clone(),
        &pipeline,
        Some(tool_router),
    )
    .await
    .unwrap();
    assert_eq!(result, TurnResult::Complete);

    let requests = provider.requests.lock().await;
    request_tool_names(requests.first().expect("at least one provider request"))
}

#[tokio::test]
async fn agentic_memory_tools_offered_only_when_gate_is_on() {
    // Pins: the harness surfaces memory_search/memory_navigate onto a turn only
    // when the memory-stage gate flag is set; a gated-off turn never sees them.
    let offered = run_capturing_turn(true).await;
    assert!(
        offered.iter().any(|name| name == "memory_search"),
        "gate-on turn must offer memory_search; tools were {offered:?}"
    );
    assert!(
        offered.iter().any(|name| name == "memory_navigate"),
        "gate-on turn must offer memory_navigate; tools were {offered:?}"
    );

    let not_offered = run_capturing_turn(false).await;
    assert!(
        !not_offered.iter().any(|name| name == "memory_search"),
        "gate-off turn must not offer memory_search; tools were {not_offered:?}"
    );
    assert!(
        !not_offered.iter().any(|name| name == "memory_navigate"),
        "gate-off turn must not offer memory_navigate; tools were {not_offered:?}"
    );
}

#[tokio::test]
async fn offered_memory_search_executes_and_returns_provenance() {
    // Pins: when offered, memory_search executes through the retrieval executor
    // and the tool result carries per-hit provenance (graph_uid, chunk_uid,
    // source_uri) so tool-derived answers stay citable.
    let config = MoaConfig::default();
    let session = session_meta("agentic-memory-exec", "claude-sonnet-4-6");
    let session_id = session.id;
    let store = Arc::new(MockSessionStore::new(session, Vec::new()));
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Investigate my API key rotation policy".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let sandbox_dir = tempdir().unwrap();
    let tool_router = Arc::new(
        ToolRouter::new_local(sandbox_dir.path())
            .await
            .unwrap()
            .with_memory_retrieval_executor(Arc::new(TestRetrievalExecutor)),
    );
    let pipeline = pipeline_with_gate(&config, store.clone(), tool_router.tool_schemas(), true);
    let provider = Arc::new(MemorySearchThenEndProvider::default());

    let result = run_brain_turn(
        session_id,
        store.clone(),
        provider.clone(),
        &pipeline,
        Some(tool_router),
    )
    .await
    .unwrap();
    assert_eq!(result, TurnResult::Complete);
    assert_eq!(
        provider.requests.lock().await.len(),
        2,
        "the model calls memory_search then ends the turn"
    );

    let events = store
        .get_events(session_id, EventRange::all())
        .await
        .unwrap();
    let tool_result = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output, success, ..
            } if *success => Some(output.clone()),
            _ => None,
        })
        .expect("memory_search must produce a tool result event");
    let structured = tool_result
        .structured
        .as_ref()
        .expect("memory_search tool output must carry a structured payload");
    let hit = &structured["hits"][0];
    assert_eq!(hit["graph_uid"], json!(GRAPH_UID));
    assert_eq!(hit["chunk_uid"], json!(CHUNK_UID));
    assert_eq!(hit["document_version_uid"], json!(DOCUMENT_VERSION_UID));
    assert_eq!(
        hit["source_uri"],
        json!("https://kb.example.invalid/rotation")
    );
}
