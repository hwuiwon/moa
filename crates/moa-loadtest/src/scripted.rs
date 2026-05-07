//! Per-session scripted LLM provider used by mock load tests.

use crate::*;

#[derive(Clone)]
pub(crate) struct PerSessionScriptedProvider {
    capabilities: ModelCapabilities,
    timing: MockProviderTiming,
    responses: Arc<StdMutex<HashMap<String, VecDeque<ScriptedResponse>>>>,
    recorded_requests: Arc<StdMutex<Vec<CompletionRequest>>>,
}

impl PerSessionScriptedProvider {
    pub(crate) fn new(capabilities: ModelCapabilities, timing: MockProviderTiming) -> Self {
        Self {
            capabilities,
            timing,
            responses: Arc::new(StdMutex::new(HashMap::new())),
            recorded_requests: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    pub(crate) fn register_session(
        &self,
        session_id: &SessionId,
        plan: &SessionPlan,
    ) -> Result<()> {
        let session_key = session_id.to_string();
        let responses = scripted_responses_for_plan(plan);
        self.responses
            .lock()
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider response registry poisoned: {error}"
                ))
            })?
            .insert(session_key, responses);
        Ok(())
    }
}

#[async_trait]
impl LLMProvider for PerSessionScriptedProvider {
    fn name(&self) -> &str {
        "scripted-per-session"
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities.clone()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        self.recorded_requests
            .lock()
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider request log poisoned: {error}"
                ))
            })?
            .push(request.clone());
        let Some(session_key) = request
            .metadata
            .get("_moa.session_id")
            .and_then(serde_json::Value::as_str)
        else {
            return completion_stream_from_scripted_response(
                &self.capabilities,
                auxiliary_scripted_response(&request),
                self.timing,
            );
        };
        let response = self
            .responses
            .lock()
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider response registry poisoned: {error}"
                ))
            })?
            .get_mut(session_key)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider has no script for session {session_key}"
                ))
            })?
            .pop_front()
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider ran out of queued responses for session {session_key}"
                ))
            })?;
        completion_stream_from_scripted_response(&self.capabilities, response, self.timing)
    }
}

pub(crate) fn auxiliary_scripted_response(request: &CompletionRequest) -> ScriptedResponse {
    if request.messages.iter().any(|message| {
        message
            .content
            .contains("Distill the following successful MOA session into a reusable Agent Skill")
    }) {
        return ScriptedResponse::text(mock_skill_markdown()).with_usage(TokenUsage {
            input_tokens_uncached: 32,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 96,
        });
    }

    ScriptedResponse::text("mock auxiliary summary").with_usage(TokenUsage {
        input_tokens_uncached: 24,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: 24,
    })
}

pub(crate) fn mock_skill_markdown() -> String {
    r#"---
name: mock-loadtest-skill
description: "Mock distilled skill for load-test auxiliary requests"
metadata:
  moa-version: "1.0"
  moa-one-liner: "Synthetic skill emitted by moa-loadtest mock mode"
  moa-estimated-tokens: "64"
---

# Mock loadtest skill

1. Reproduce the target workflow with deterministic mock inputs.
2. Measure turn latency, cache hit rate, and tool activity.
3. Verify that all sessions finish without unexpected pauses.
"#
    .to_string()
}

pub(crate) fn completion_stream_from_scripted_response(
    capabilities: &ModelCapabilities,
    response: ScriptedResponse,
    timing: MockProviderTiming,
) -> Result<CompletionStream> {
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            CompletionContent::Text(text) => Some(text.as_str()),
            CompletionContent::ToolCall(_) | CompletionContent::ProviderToolResult { .. } => None,
        })
        .collect::<String>();
    let output_tokens = response
        .content
        .iter()
        .map(|block| match block {
            CompletionContent::Text(text) => text.chars().count().div_ceil(4),
            CompletionContent::ToolCall(call) => {
                8 + call
                    .invocation
                    .input
                    .to_string()
                    .chars()
                    .count()
                    .div_ceil(4)
            }
            CompletionContent::ProviderToolResult { summary, .. } => {
                summary.chars().count().div_ceil(4)
            }
        })
        .sum();

    let duration_ms = if timing.total.is_zero() {
        response.duration_ms
    } else {
        timing.total.as_millis().min(u128::from(u64::MAX)) as u64
    };
    let completion_response = CompletionResponse {
        text,
        content: response.content,
        stop_reason: response.stop_reason,
        model: capabilities.model_id.clone(),
        usage: TokenUsage {
            input_tokens_uncached: response
                .input_tokens
                .saturating_sub(response.cached_input_tokens)
                .saturating_sub(response.cache_write_input_tokens),
            input_tokens_cache_write: response.cache_write_input_tokens,
            input_tokens_cache_read: response.cached_input_tokens,
            output_tokens,
        },
        duration_ms,
        thought_signature: None,
    };

    if timing.ttft.is_zero() && timing.total.is_zero() {
        return Ok(CompletionStream::from_response(completion_response));
    }

    let buffered_blocks = completion_response.content.clone();
    let capacity = buffered_blocks.len().max(1);
    let (tx, rx) = mpsc::channel(capacity);
    let completion = tokio::spawn(async move {
        if !timing.ttft.is_zero() {
            tokio::time::sleep(timing.ttft).await;
        }
        for block in buffered_blocks {
            if tx.send(Ok(block)).await.is_err() {
                break;
            }
        }

        let remaining = timing.total.saturating_sub(timing.ttft);
        if !remaining.is_zero() {
            tokio::time::sleep(remaining).await;
        }

        Ok(completion_response)
    });

    Ok(CompletionStream::new(rx, completion))
}

pub(crate) fn scripted_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("scripted-loadtest"),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: Some(Duration::from_secs(300)),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: Some(0.0),
        },
        native_tools: Vec::new(),
    }
}

pub(crate) fn scripted_responses_for_plan(plan: &SessionPlan) -> VecDeque<ScriptedResponse> {
    let mut responses = VecDeque::new();

    for (turn_index, turn) in plan.turns.iter().enumerate() {
        match &turn.mock_behavior {
            MockTurnBehavior::Simple => {
                responses.push_back(scripted_text_response(
                    format!("mock turn {} complete", turn_index + 1),
                    turn_index,
                ));
            }
            MockTurnBehavior::FileRead {
                path,
                start_line,
                end_line,
            } => {
                let tool_id = Uuid::now_v7().to_string();
                responses.push_back(
                    ScriptedResponse::tool_call(
                        "file_read",
                        serde_json::json!({
                            "path": path,
                            "start_line": start_line,
                            "end_line": end_line,
                        }),
                        tool_id,
                    )
                    .with_usage(TokenUsage {
                        input_tokens_uncached: 20,
                        input_tokens_cache_write: 0,
                        input_tokens_cache_read: if turn_index == 0 { 0 } else { 48 },
                        output_tokens: 0,
                    }),
                );
                responses.push_back(scripted_text_response(
                    format!("mock tool turn {} complete", turn_index + 1),
                    turn_index,
                ));
            }
            #[cfg(test)]
            MockTurnBehavior::Bash { cmd } => {
                let tool_id = Uuid::now_v7().to_string();
                responses.push_back(
                    ScriptedResponse::tool_call(
                        "bash",
                        serde_json::json!({
                            "cmd": cmd,
                        }),
                        tool_id,
                    )
                    .with_usage(TokenUsage {
                        input_tokens_uncached: 24,
                        input_tokens_cache_write: 0,
                        input_tokens_cache_read: if turn_index == 0 { 0 } else { 52 },
                        output_tokens: 0,
                    }),
                );
                responses.push_back(scripted_text_response(
                    format!("mock approval turn {} complete", turn_index + 1),
                    turn_index,
                ));
            }
        }
    }

    responses
}

pub(crate) fn scripted_text_response(text: String, turn_index: usize) -> ScriptedResponse {
    ScriptedResponse::text(text).with_usage(TokenUsage {
        input_tokens_uncached: if turn_index == 0 { 64 } else { 20 },
        input_tokens_cache_write: 0,
        input_tokens_cache_read: if turn_index == 0 { 0 } else { 44 },
        output_tokens: 24,
    })
}
