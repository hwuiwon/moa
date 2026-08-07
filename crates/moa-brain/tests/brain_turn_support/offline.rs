// Offline brain-turn integration-test support.

use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::{
    TurnResult, pipeline::history::HistoryCompiler, run_brain_turn, run_streamed_turn,
    runtime_events::RuntimeEvent,
};
use moa_config::MoaConfig;
use moa_core::{
    error::Result, events::Event, events::EventType, traits::LLMProvider, traits::SessionStore,
    types::action_policy::ActionPolicyEffect, types::action_policy::ActionPolicyRule,
    types::action_policy::ActionRuleScope, types::completion::CompletionContent,
    types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::completion::CompletionStream,
    types::completion::SharedCompletionRequest, types::completion::StopReason,
    types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::contact::SessionActorRef, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::SessionId, types::identifiers::UserId,
    types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    types::session::SessionMeta,
};
use moa_hands::ToolRouter;
use moa_security::ActionPolicyRuleStore;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::{Mutex, broadcast};

fn approximate_tokens(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

struct MockLlmProvider;

impl MockLlmProvider {
    fn response() -> CompletionStream {
        CompletionStream::from_response(CompletionResponse {
            text: "Hi there".to_string(),
            content: vec![moa_core::types::completion::CompletionContent::Text(
                "Hi there".to_string(),
            )],
            stop_reason: StopReason::EndTurn,
            model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
            usage: token_usage(32, 8),
            duration_ms: 25,
            thought_signature: None,
        })
    }
}

#[async_trait]
impl LLMProvider for MockLlmProvider {
    fn name(&self) -> &str {
        "mock"
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

    async fn complete(
        &self,
        _request: SharedCompletionRequest,
    ) -> Result<CompletionStream> {
        Ok(Self::response())
    }
}

#[derive(Clone)]
struct CapturingTextLlmProvider {
    text: String,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl CapturingTextLlmProvider {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn response(&self) -> CompletionStream {
        CompletionStream::from_response(CompletionResponse {
            text: self.text.clone(),
            content: vec![moa_core::types::completion::CompletionContent::Text(
                self.text.clone(),
            )],
            stop_reason: StopReason::EndTurn,
            model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
            usage: token_usage(32, 8),
            duration_ms: 25,
            thought_signature: None,
        })
    }
}

#[async_trait]
impl LLMProvider for CapturingTextLlmProvider {
    fn name(&self) -> &str {
        "capturing-text"
    }

    fn capabilities(&self) -> ModelCapabilities {
        MockLlmProvider.capabilities()
    }

    async fn complete(
        &self,
        request: SharedCompletionRequest,
    ) -> Result<CompletionStream> {
        self.requests
            .lock()
            .await
            .push(CompletionRequest::from_view(&request));
        Ok(self.response())
    }
}

#[derive(Clone)]
struct PartialUsageToolLlmProvider {
    usage: TokenUsage,
    requests: Arc<Mutex<Vec<()>>>,
}

impl PartialUsageToolLlmProvider {
    fn new(usage: TokenUsage) -> Self {
        Self {
            usage,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn response(&self) -> CompletionStream {
        CompletionStream::from_response(CompletionResponse {
            text: String::new(),
            content: vec![CompletionContent::ToolCall(ToolCallContent {
                invocation: ToolInvocation {
                    id: Some("missing-usage-tool-call".to_string()),
                    name: "bash".to_string(),
                    input: json!({ "cmd": "printf must-not-run" }),
                },
                provider_metadata: None,
            })],
            stop_reason: StopReason::ToolUse,
            model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
            usage: self.usage,
            duration_ms: 10,
            thought_signature: None,
        })
    }
}

#[async_trait]
impl LLMProvider for PartialUsageToolLlmProvider {
    fn name(&self) -> &str {
        "partial-usage-tool"
    }

    fn capabilities(&self) -> ModelCapabilities {
        MockLlmProvider.capabilities()
    }

    async fn complete(
        &self,
        _request: SharedCompletionRequest,
    ) -> Result<CompletionStream> {
        self.requests.lock().await.push(());
        Ok(self.response())
    }
}

#[derive(Clone, Default)]
struct SubMicroToolLoopLlmProvider {
    requests: Arc<Mutex<Vec<()>>>,
}

impl SubMicroToolLoopLlmProvider {
    async fn complete_response(&self) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("sub-micro-tool-call".to_string()),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "printf submicro" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(1, 1),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            CompletionResponse {
                text: "must not reach the second response".to_string(),
                content: vec![CompletionContent::Text(
                    "must not reach the second response".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(1, 1),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }
}

#[async_trait]
impl LLMProvider for SubMicroToolLoopLlmProvider {
    fn name(&self) -> &str {
        "sub-micro-tool-loop"
    }

    fn capabilities(&self) -> ModelCapabilities {
        let mut capabilities = MockLlmProvider.capabilities();
        capabilities.pricing = TokenPricing {
            input_per_mtok: 0.000_001,
            output_per_mtok: 0.000_001,
            cached_input_per_mtok: Some(0.000_001),
            cache_write_5m_per_mtok: Some(0.000_001),
            cache_write_1h_per_mtok: None,
        };
        capabilities
    }

    async fn complete(
        &self,
        _request: SharedCompletionRequest,
    ) -> Result<CompletionStream> {
        self.complete_response().await
    }
}

#[derive(Clone, Default)]
struct CappedOutputLlmProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for CappedOutputLlmProvider {
    fn name(&self) -> &str {
        "capped-output"
    }

    fn capabilities(&self) -> ModelCapabilities {
        let mut capabilities = MockLlmProvider.capabilities();
        capabilities.pricing = TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 1.0,
            cached_input_per_mtok: Some(0.0),
            cache_write_5m_per_mtok: Some(0.0),
            cache_write_1h_per_mtok: None,
        };
        capabilities
    }

    async fn complete(
        &self,
        request: SharedCompletionRequest,
    ) -> Result<CompletionStream> {
        self.requests
            .lock()
            .await
            .push(CompletionRequest::from_view(&request));
        Ok(Self::response())
    }
}

impl CappedOutputLlmProvider {
    fn response() -> CompletionStream {
        CompletionStream::from_response(CompletionResponse {
            text: "bounded response".to_string(),
            content: vec![CompletionContent::Text("bounded response".to_string())],
            stop_reason: StopReason::EndTurn,
            model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
            usage: token_usage(3, 2),
            duration_ms: 10,
            thought_signature: None,
        })
    }
}

#[derive(Default)]
struct ToolLoopLlmProvider {
    requests: Arc<Mutex<Vec<()>>>,
}

#[async_trait]
impl LLMProvider for ToolLoopLlmProvider {
    fn name(&self) -> &str {
        "mock-tool-loop"
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

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("11111111-1111-1111-1111-111111111111".to_string()),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "printf 'hello from tool'" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(12, 5),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("hello from tool"))
            );
            CompletionResponse {
                text: "Tool said hello from tool".to_string(),
                content: vec![CompletionContent::Text(
                    "Tool said hello from tool".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(20, 7),
                duration_ms: 12,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }

}

struct PolicyBlockedToolLlmProvider {
    tool_id: &'static str,
    expected_error_fragment: &'static str,
    final_text: &'static str,
    requests: Arc<Mutex<Vec<()>>>,
}

impl PolicyBlockedToolLlmProvider {
    fn new(
        tool_id: &'static str,
        expected_error_fragment: &'static str,
        final_text: &'static str,
    ) -> Self {
        Self {
            tool_id,
            expected_error_fragment,
            final_text,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl LLMProvider for PolicyBlockedToolLlmProvider {
    fn name(&self) -> &str {
        "mock-policy-blocked-tool"
    }

    fn capabilities(&self) -> ModelCapabilities {
        MockLlmProvider.capabilities()
    }

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some(self.tool_id.to_string()),
                        name: "file_write".to_string(),
                        input: json!({
                            "path": "blocked-policy-write.txt",
                            "content": "must-not-be-written"
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(12, 5),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            assert!(
                request.messages.iter().any(|message| {
                    message.tool_use_id.as_deref() == Some(self.tool_id)
                        && message.content.contains(self.expected_error_fragment)
                }),
                "expected follow-up request to include blocked tool error `{}`; messages were: {:#?}",
                self.expected_error_fragment,
                request.messages
            );
            CompletionResponse {
                text: self.final_text.to_string(),
                content: vec![CompletionContent::Text(self.final_text.to_string())],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(16, 6),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }

}

#[derive(Default)]
struct LargeToolOutputLlmProvider {
    requests: Arc<Mutex<Vec<()>>>,
}

#[async_trait]
impl LLMProvider for LargeToolOutputLlmProvider {
    fn name(&self) -> &str {
        "large-tool-output"
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

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("22222222-2222-2222-2222-222222222222".to_string()),
                        name: "bash".to_string(),
                        input: json!({
                            "cmd": "python3 -c \"print('x' * 120000)\""
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(14, 6),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("[output truncated from ~")),
                "expected truncated tool result in replayed context; request was: {request:?}"
            );
            CompletionResponse {
                text: "Large tool output handled".to_string(),
                content: vec![CompletionContent::Text(
                    "Large tool output handled".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(18, 5),
                duration_ms: 11,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }

}

#[derive(Default)]
struct OpenAiToolLoopLlmProvider {
    requests: Arc<Mutex<Vec<()>>>,
}

#[async_trait]
impl LLMProvider for OpenAiToolLoopLlmProvider {
    fn name(&self) -> &str {
        "openai-tool-loop"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 1.25,
                output_per_mtok: 10.0,
                cached_input_per_mtok: Some(0.125),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("fc_action_1".to_string()),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "printf 'hello from openai tool'" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
                usage: token_usage(12, 5),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            let tool_result = request.messages.iter().find(|message| {
                message.role == moa_core::types::context::MessageRole::Tool
                    && message.tool_use_id.as_deref() == Some("fc_action_1")
            });
            assert!(
                tool_result.is_some(),
                "expected function_call_output for fc_action_1 after tool execution; request was: {request:?}"
            );
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| { message.content.contains("hello from openai tool") }),
                "expected tool output to be preserved after execution; request was: {request:?}"
            );
            CompletionResponse {
                text: "Tool completed".to_string(),
                content: vec![CompletionContent::Text("Tool completed".to_string())],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
                usage: token_usage(20, 7),
                duration_ms: 12,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }

}

#[derive(Default)]
struct OpenAiFailedReadLoopLlmProvider {
    requests: Arc<Mutex<Vec<()>>>,
}

#[async_trait]
impl LLMProvider for OpenAiFailedReadLoopLlmProvider {
    fn name(&self) -> &str {
        "openai-failed-read-loop"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 1.25,
                output_per_mtok: 10.0,
                cached_input_per_mtok: Some(0.125),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("fc_failed_read_1".to_string()),
                        name: "file_read".to_string(),
                        input: json!({ "path": "../secret.txt" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
                usage: token_usage(12, 5),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            assert!(
                request.messages.iter().any(|message| {
                    message
                        .tool_invocation
                        .as_ref()
                        .and_then(|invocation| invocation.id.as_deref())
                        == Some("fc_failed_read_1")
                }),
                "expected assistant function_call history for fc_failed_read_1; request was: {request:?}"
            );
            assert!(
                request.messages.iter().any(|message| {
                    message.role == moa_core::types::context::MessageRole::Tool
                        && message.tool_use_id.as_deref() == Some("fc_failed_read_1")
                        && message.content.contains("path traversal")
                }),
                "expected function_call_output for fc_failed_read_1; request was: {request:?}"
            );
            CompletionResponse {
                text: "Read failed as expected".to_string(),
                content: vec![CompletionContent::Text(
                    "Read failed as expected".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("gpt-5.4"),
                usage: token_usage(20, 7),
                duration_ms: 12,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }

}

#[derive(Default)]
struct CanaryLeakLlmProvider {
    requests: Arc<Mutex<Vec<()>>>,
}

#[async_trait]
impl LLMProvider for CanaryLeakLlmProvider {
    fn name(&self) -> &str {
        "mock-canary-leak"
    }

    fn capabilities(&self) -> ModelCapabilities {
        MockLlmProvider.capabilities()
    }

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            let canary = request
                .messages
                .iter()
                .filter(|message| message.role == moa_core::types::context::MessageRole::System)
                .find_map(|message| {
                    message.content.split_whitespace().find_map(|token| {
                        token
                            .contains("moa_canary_")
                            .then(|| token.trim_matches('`').to_string())
                    })
                })
                .expect("missing injected canary");
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("33333333-3333-3333-3333-333333333333".to_string()),
                        name: "file_read".to_string(),
                        input: json!({ "path": format!("{canary}.txt") }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(20, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            assert!(request.messages.iter().any(|message| matches!(
                message.role,
                moa_core::types::context::MessageRole::System
                    | moa_core::types::context::MessageRole::Tool
            ) && message.content.contains("canary")));
            CompletionResponse {
                text: "blocked".to_string(),
                content: vec![CompletionContent::Text("blocked".to_string())],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(16, 2),
                duration_ms: 8,
                thought_signature: None,
            }
        };
        requests.push(());
        Ok(CompletionStream::from_response(response))
    }

}

#[derive(Default)]
struct MaliciousToolOutputLlmProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for MaliciousToolOutputLlmProvider {
    fn name(&self) -> &str {
        "mock-malicious-tool-output"
    }

    fn capabilities(&self) -> ModelCapabilities {
        MockLlmProvider.capabilities()
    }

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let mut requests = self.requests.lock().await;
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("44444444-4444-4444-4444-444444444444".to_string()),
                        name: "file_read".to_string(),
                        input: json!({ "path": "unsafe.txt" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(18, 3),
                duration_ms: 12,
                thought_signature: None,
            }
        } else {
            let tool_message = request
                .messages
                .iter()
                .find(|message| message.role == moa_core::types::context::MessageRole::Tool)
                .expect("missing tool result message");
            assert!(
                tool_message.content.contains("<untrusted_tool_output>"),
                "{}",
                tool_message.content
            );
            // The classifier destroyed the attempt at the raw-output source, so the
            // wrapper now carries the fixed safe replacement rather than the payload.
            assert!(
                !tool_message
                    .content
                    .contains("ignore previous instructions"),
                "{}",
                tool_message.content
            );
            assert!(
                tool_message.content.contains("tool output withheld"),
                "{}",
                tool_message.content
            );
            CompletionResponse {
                text: "wrapped".to_string(),
                content: vec![CompletionContent::Text("wrapped".to_string())],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
                usage: token_usage(22, 5),
                duration_ms: 11,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }

}

struct ProviderToolResultTurnLlm;

impl ProviderToolResultTurnLlm {
    fn response() -> CompletionStream {
        CompletionStream::from_response(CompletionResponse {
            text: "Fresh answer from web search".to_string(),
            content: vec![
                CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: "Searching the web...".to_string(),
                },
                CompletionContent::Text("Fresh answer from web search".to_string()),
            ],
            stop_reason: StopReason::EndTurn,
            model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
            usage: token_usage(8, 5),
            duration_ms: 6,
            thought_signature: None,
        })
    }
}

#[async_trait]
impl LLMProvider for ProviderToolResultTurnLlm {
    fn name(&self) -> &str {
        "mock-provider-tool-result-turn"
    }

    fn capabilities(&self) -> ModelCapabilities {
        MockLlmProvider.capabilities()
    }

    async fn complete(
        &self,
        _request: SharedCompletionRequest,
    ) -> Result<CompletionStream> {
        Ok(Self::response())
    }
}

fn make_event_record(session_id: &SessionId, sequence_num: u64, event: Event) -> EventRecord {
    EventRecord {
        id: uuid::Uuid::now_v7(),
        session_id: *session_id,
        sequence_num,
        event_type: event.event_type(),
        event,
        timestamp: moa_test_support::fixtures::pg_now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    }
}

struct StaticActionPolicyRuleStore {
    rules: Vec<ActionPolicyRule>,
}

#[async_trait]
impl ActionPolicyRuleStore for StaticActionPolicyRuleStore {
    async fn list_action_policy_rules_for_tool(
        &self,
        tenant_id: &moa_core::types::identifiers::TenantId,
        _user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>> {
        Ok(self
            .rules
            .iter()
            .filter(|rule| {
                rule.tool == tool
                    && matches!(
                        rule.scope,
                        ActionRuleScope::Tenant { tenant_id: rule_tenant_id }
                            if rule_tenant_id == *tenant_id
                    )
            })
            .cloned()
            .collect())
    }

    async fn upsert_action_policy_rule(&self, _rule: ActionPolicyRule) -> Result<()> {
        Ok(())
    }

    async fn delete_action_policy_rule(
        &self,
        _tenant_id: &moa_core::types::identifiers::TenantId,
        _user_id: Option<&UserId>,
        _tool: &str,
        _pattern: &str,
    ) -> Result<()> {
        Ok(())
    }
}

fn allow_bash_commands_for_tenant<const N: usize>(
    tenant_id: moa_core::types::identifiers::TenantId,
    patterns: [&str; N],
) -> Arc<dyn ActionPolicyRuleStore> {
    Arc::new(StaticActionPolicyRuleStore {
        rules: patterns
            .into_iter()
            .map(|pattern| ActionPolicyRule {
                id: uuid::Uuid::now_v7(),
                scope: ActionRuleScope::Tenant { tenant_id },
                tool: "bash".to_string(),
                pattern: pattern.to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("offline test fixture allows this exact command".to_string()),
                created_by: UserId::new("offline-test-admin"),
                created_at: moa_test_support::fixtures::pg_now(),
            })
            .collect(),
    })
}
