// Session-search brain-turn integration-test support.

use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::{TurnResult, build_default_pipeline_with_tools, run_brain_turn};
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, Event, EventRange,
    LLMProvider, MoaConfig, ModelCapabilities, Result, SessionActorRef, SessionId, SessionMeta,
    SessionStore, StopReason, TokenPricing, ToolCallContent, ToolCallFormat, ToolCallId,
    ToolInvocation, ToolOutput, UserId,
};
use moa_hands::ToolRouter;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::Mutex;

fn filler_text(label: &str, count: usize) -> String {
    format!("{label} {}", "x".repeat(count))
}

fn count_lines(text: &str) -> usize {
    text.lines().count()
}

fn extract_tool_id_field(message: &str) -> Option<String> {
    let marker = "tool_id=";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let candidate = &rest[..rest.len().min(36)];
    if uuid::Uuid::parse_str(candidate).is_ok() {
        Some(candidate.to_string())
    } else {
        None
    }
}

struct SessionSearchArtifactLlmProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    expected_tool_id: ToolCallId,
}

impl SessionSearchArtifactLlmProvider {
    fn new(expected_tool_id: ToolCallId) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            expected_tool_id,
        }
    }
}

#[async_trait]
impl LLMProvider for SessionSearchArtifactLlmProvider {
    fn name(&self) -> &str {
        "session-search-artifact"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: moa_core::ModelId::new("claude-sonnet-4-6"),
            context_window: 8_000,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().await;
        let response = match requests.len() {
            0 => {
                assert!(
                    !request.messages.iter().any(|message| message
                        .content
                        .contains(&self.expected_tool_id.to_string())),
                    "expected old tool id to be absent from active context so the model must use session_search; request was: {request:?}"
                );
                CompletionResponse {
                    text: String::new(),
                    content: vec![CompletionContent::ToolCall(ToolCallContent {
                        invocation: ToolInvocation {
                            id: Some("77777777-7777-7777-7777-777777777777".to_string()),
                            name: "session_search".to_string(),
                            input: json!({
                                "query": "bash",
                                "event_type": "tool_call",
                                "last_n": 5,
                            }),
                        },
                        provider_metadata: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    model: moa_core::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(19, 9),
                    duration_ms: 10,
                    thought_signature: None,
                }
            }
            1 => {
                let session_search_message = request
                    .messages
                    .iter()
                    .find(|message| message.content.contains("## #"))
                    .unwrap_or_else(|| {
                        panic!(
                            "expected session_search output in context; request was: {request:?}"
                        )
                    });
                let tool_id = extract_tool_id_field(&session_search_message.content)
                    .expect("tool id from session_search");
                assert_eq!(tool_id, self.expected_tool_id.to_string());
                CompletionResponse {
                    text: String::new(),
                    content: vec![CompletionContent::ToolCall(ToolCallContent {
                        invocation: ToolInvocation {
                            id: Some("88888888-8888-8888-8888-888888888888".to_string()),
                            name: "tool_result_search".to_string(),
                            input: json!({
                                "tool_id": tool_id,
                                "pattern": "bash-line-140-",
                                "literal": true,
                            }),
                        },
                        provider_metadata: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    model: moa_core::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(22, 10),
                    duration_ms: 11,
                    thought_signature: None,
                }
            }
            _ => {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("bash-line-140-")),
                    "expected tool_result_search output in replayed context; request was: {request:?}"
                );
                CompletionResponse {
                    text: "Recovered old artifact via session_search and tool_result_search"
                        .to_string(),
                    content: vec![CompletionContent::Text(
                        "Recovered old artifact via session_search and tool_result_search"
                            .to_string(),
                    )],
                    stop_reason: StopReason::EndTurn,
                    model: moa_core::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(25, 9),
                    duration_ms: 12,
                    thought_signature: None,
                }
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

#[derive(Default)]
struct RepeatingToolLlmProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for RepeatingToolLlmProvider {
    fn name(&self) -> &str {
        "mock-repeating-tool-loop"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: moa_core::ModelId::new("claude-sonnet-4-6"),
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().await;
        let request_index = requests.len();
        let response = match request_index {
            0 | 2 => CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some(format!(
                            "00000000-0000-0000-0000-00000000000{}",
                            request_index + 1
                        )),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "printf 'hello from tool'" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(12, 5),
                duration_ms: 10,
                thought_signature: None,
            },
            1 | 3 => {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("hello from tool"))
                );
                CompletionResponse {
                    text: format!("Tool said hello from tool ({request_index})"),
                    content: vec![CompletionContent::Text(format!(
                        "Tool said hello from tool ({request_index})"
                    ))],
                    stop_reason: StopReason::EndTurn,
                    model: moa_core::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(20, 7),
                    duration_ms: 12,
                    thought_signature: None,
                }
            }
            _ => CompletionResponse {
                text: "done".to_string(),
                content: vec![CompletionContent::Text("done".to_string())],
                stop_reason: StopReason::EndTurn,
                model: moa_core::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(10, 2),
                duration_ms: 5,
                thought_signature: None,
            },
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}
