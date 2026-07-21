// Artifact-backed tool-output brain-turn integration-test support.

use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::{TurnResult, run_brain_turn};
use moa_core::{types::completion::CompletionContent, types::completion::CompletionRequest, types::completion::CompletionResponse, types::completion::CompletionStream, events::Event, types::events_stream::EventRange, traits::LLMProvider, types::model::ModelCapabilities, error::Result, types::contact::SessionActorRef, types::identifiers::SessionId, types::session::SessionMeta, traits::SessionStore, types::completion::StopReason, types::model::TokenPricing, types::completion::ToolCallContent, types::model::ToolCallFormat, types::completion::ToolInvocation};
use moa_config::MoaConfig;
use moa_hands::ToolRouter;
use moa_security::ActionPolicies;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::Mutex;

const ARTIFACT_RETRIEVAL_BASH_CMD: &str =
    "python3 -c \"for i in range(1, 261): print(f'bash-line-{i}-' + ('x' * 120))\"";
const ARTIFACT_STDERR_BASH_CMD: &str = "python3 -c \"import sys; [print(f'stdout-line-{i}-' + ('x' * 120)) for i in range(1, 261)]; sys.stderr.write('warning: deprecated config\\nwarning: retrying fallback\\n')\"";

#[derive(Default)]
struct ArtifactRetrievalLlmProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for ArtifactRetrievalLlmProvider {
    fn name(&self) -> &str {
        "artifact-retrieval"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
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
        let response = match requests.len() {
            0 => CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("33333333-3333-3333-3333-333333333333".to_string()),
                        name: "bash".to_string(),
                        input: json!({
                            "cmd": ARTIFACT_RETRIEVAL_BASH_CMD
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(18, 8),
                duration_ms: 10,
                thought_signature: None,
            },
            1 => {
                let artifact_message = request
                    .messages
                    .iter()
                    .find(|message| {
                        message.content.contains("<tool_result id=\"")
                            && message.content.contains("artifact=\"stored\"")
                    })
                    .unwrap_or_else(|| {
                        panic!("expected artifact-backed tool result, request was: {request:?}")
                    });
                assert!(
                    !artifact_message.content.contains("bash-line-140"),
                    "artifact replay should not inline the middle of the large bash output"
                );
                assert!(
                    artifact_message.content.contains("tool_result_search"),
                    "artifact summary should advertise retrieval tools"
                );
                let tool_id = extract_tool_result_id(&artifact_message.content)
                    .expect("tool result id should be present");

                CompletionResponse {
                    text: String::new(),
                    content: vec![CompletionContent::ToolCall(ToolCallContent {
                        invocation: ToolInvocation {
                            id: Some("44444444-4444-4444-4444-444444444444".to_string()),
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
                    model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
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
                    text: "Recovered bash-line-140 via tool_result_search".to_string(),
                    content: vec![CompletionContent::Text(
                        "Recovered bash-line-140 via tool_result_search".to_string(),
                    )],
                    stop_reason: StopReason::EndTurn,
                    model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(26, 9),
                    duration_ms: 12,
                    thought_signature: None,
                }
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

fn extract_tool_result_id(message: &str) -> Option<String> {
    let marker = "<tool_result id=\"";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[derive(Default)]
struct ArtifactStderrLlmProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for ArtifactStderrLlmProvider {
    fn name(&self) -> &str {
        "artifact-stderr"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
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
        let response = match requests.len() {
            0 => CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("55555555-5555-5555-5555-555555555555".to_string()),
                        name: "bash".to_string(),
                        input: json!({
                            "cmd": ARTIFACT_STDERR_BASH_CMD
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(18, 9),
                duration_ms: 10,
                thought_signature: None,
            },
            1 => {
                let artifact_message = request
                    .messages
                    .iter()
                    .find(|message| message.content.contains("artifact_streams=\"combined,stdout,stderr\""))
                    .unwrap_or_else(|| panic!("expected artifact-backed stderr-capable tool result, request was: {request:?}"));
                let tool_id =
                    extract_tool_result_id(&artifact_message.content).expect("tool result id");
                CompletionResponse {
                    text: String::new(),
                    content: vec![CompletionContent::ToolCall(ToolCallContent {
                        invocation: ToolInvocation {
                            id: Some("66666666-6666-6666-6666-666666666666".to_string()),
                            name: "tool_result_read".to_string(),
                            input: json!({
                                "tool_id": tool_id,
                                "stream": "stderr",
                                "start_line": 1,
                                "end_line": 5,
                            }),
                        },
                        provider_metadata: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(20, 8),
                    duration_ms: 11,
                    thought_signature: None,
                }
            }
            _ => {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("warning: retrying fallback")),
                    "expected stderr retrieval in replayed context; request was: {request:?}"
                );
                CompletionResponse {
                    text: "stderr warning recovered via tool_result_read".to_string(),
                    content: vec![CompletionContent::Text(
                        "stderr warning recovered via tool_result_read".to_string(),
                    )],
                    stop_reason: StopReason::EndTurn,
                    model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                    usage: token_usage(24, 8),
                    duration_ms: 12,
                    thought_signature: None,
                }
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}
