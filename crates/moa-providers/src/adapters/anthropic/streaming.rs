//! Anthropic SSE stream normalization.

use super::response::stop_reason_from_anthropic;
use super::tools::{
    WebSearchResultContent, summarize_anthropic_search_results, summarize_anthropic_server_tool_use,
};
use super::*;

pub(super) async fn consume_sse_events<S, E>(
    events: S,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    span_recorder: &mut LLMSpanRecorder,
) -> Result<CompletionResponse>
where
    S: Stream<Item = std::result::Result<SseEvent, E>>,
    E: std::fmt::Display,
{
    let mut state = AnthropicStreamState::new(fallback_model);
    pin_mut!(events);

    while let Some(event) = events.next().await {
        let event = event
            .map_err(|error| MoaError::StreamError(format!("failed to read SSE event: {error}")))?;
        let emitted = state.apply_event(&event)?;

        for block in emitted {
            span_recorder.observe_block(&block);
            if tx.send(Ok(block)).await.is_err() {
                tracing::debug!("completion stream receiver dropped before the response finished");
                break;
            }
        }
    }

    if !state.saw_message_stop {
        return Err(MoaError::StreamError(
            "Anthropic stream ended before the provider returned message_stop".to_string(),
        ));
    }
    span_recorder.set_cached_input_tokens(state.cached_input_tokens);
    span_recorder.set_cache_creation_input_tokens(state.cache_creation_input_tokens);
    Ok(state.finish(started_at))
}

#[derive(Debug)]
struct AnthropicStreamState {
    model: String,
    stop_reason: StopReason,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    cache_creation_input_tokens: usize,
    saw_message_stop: bool,
    blocks: Vec<BlockAccumulator>,
    completed_content: Vec<Option<CompletionContent>>,
}

impl AnthropicStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            stop_reason: StopReason::Other("unknown".to_string()),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            saw_message_stop: false,
            blocks: Vec::new(),
            completed_content: Vec::new(),
        }
    }

    fn apply_event(&mut self, event: &SseEvent) -> Result<Vec<CompletionContent>> {
        match event.event.as_str() {
            "message_start" => {
                let payload: MessageStartEvent = parse_sse_json(event)?;
                self.model = payload.message.model;
                if let Some(usage) = payload.message.usage {
                    self.input_tokens = usage.input_tokens;
                    self.cached_input_tokens = usage.cache_read_input_tokens;
                    self.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                }
                Ok(Vec::new())
            }
            "content_block_start" => self.apply_block_start(parse_sse_json(event)?),
            "content_block_delta" => self.apply_block_delta(parse_sse_json(event)?),
            "content_block_stop" => Ok(self.apply_block_stop(parse_sse_json(event)?)),
            "message_delta" => {
                let payload: MessageDeltaEvent = parse_sse_json(event)?;
                self.stop_reason = payload
                    .delta
                    .stop_reason
                    .map(stop_reason_from_anthropic)
                    .unwrap_or_else(|| StopReason::Other("unknown".to_string()));
                if let Some(usage) = payload.usage {
                    self.output_tokens = usage.output_tokens;
                    if usage.cache_read_input_tokens > 0 {
                        self.cached_input_tokens = usage.cache_read_input_tokens;
                    }
                    if usage.cache_creation_input_tokens > 0 {
                        self.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                    }
                }
                Ok(Vec::new())
            }
            "message_stop" => {
                self.saw_message_stop = true;
                Ok(Vec::new())
            }
            "ping" => Ok(Vec::new()),
            "error" => {
                let payload: ErrorEvent = parse_sse_json(event)?;
                Err(MoaError::ProviderError(format!(
                    "Anthropic stream error ({}): {}",
                    payload.error.kind, payload.error.message
                )))
            }
            _ => {
                tracing::debug!(event = %event.event, "ignoring unknown Anthropic SSE event");
                Ok(Vec::new())
            }
        }
    }

    fn apply_block_start(
        &mut self,
        payload: ContentBlockStartEvent,
    ) -> Result<Vec<CompletionContent>> {
        self.ensure_capacity(payload.index);
        match payload.content_block {
            ContentBlockStart::Text { text } => {
                self.blocks[payload.index] = BlockAccumulator::Text(text.clone());
                if text.is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(vec![CompletionContent::Text(text)])
                }
            }
            ContentBlockStart::ToolUse { id, name, input } => {
                let partial_json = initial_tool_input(input)?;
                self.blocks[payload.index] = BlockAccumulator::Tool {
                    id,
                    name,
                    partial_json,
                };
                Ok(Vec::new())
            }
            ContentBlockStart::ServerToolUse { _id: _, name } => {
                self.blocks[payload.index] = BlockAccumulator::ServerTool {
                    name,
                    partial_json: String::new(),
                };
                Ok(Vec::new())
            }
            ContentBlockStart::WebSearchToolResult {
                _tool_use_id: _,
                content,
            } => {
                self.blocks[payload.index] = BlockAccumulator::Ignored;
                self.ensure_completed_capacity(payload.index);
                let block = CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: summarize_anthropic_search_results(&content),
                };
                self.completed_content[payload.index] = Some(block.clone());
                Ok(vec![block])
            }
            ContentBlockStart::Unknown => {
                self.blocks[payload.index] = BlockAccumulator::Ignored;
                Ok(Vec::new())
            }
        }
    }

    fn apply_block_delta(
        &mut self,
        payload: ContentBlockDeltaEvent,
    ) -> Result<Vec<CompletionContent>> {
        self.ensure_capacity(payload.index);
        match (&mut self.blocks[payload.index], payload.delta) {
            (BlockAccumulator::Text(text), ContentBlockDelta::TextDelta { text: delta }) => {
                text.push_str(&delta);
                Ok(vec![CompletionContent::Text(delta)])
            }
            (
                BlockAccumulator::Tool { partial_json, .. },
                ContentBlockDelta::InputJsonDelta {
                    partial_json: delta,
                },
            ) => {
                partial_json.push_str(&delta);
                Ok(Vec::new())
            }
            (
                BlockAccumulator::ServerTool { partial_json, .. },
                ContentBlockDelta::InputJsonDelta {
                    partial_json: delta,
                },
            ) => {
                partial_json.push_str(&delta);
                Ok(Vec::new())
            }
            (_, ContentBlockDelta::Unknown) => Ok(Vec::new()),
            _ => Err(MoaError::StreamError(
                "received an Anthropic content delta that did not match the active block"
                    .to_string(),
            )),
        }
    }

    fn apply_block_stop(&mut self, payload: ContentBlockStopEvent) -> Vec<CompletionContent> {
        self.ensure_capacity(payload.index);
        self.ensure_completed_capacity(payload.index);

        let block = std::mem::replace(&mut self.blocks[payload.index], BlockAccumulator::Ignored);
        match block {
            BlockAccumulator::Text(text) => {
                self.completed_content[payload.index] = Some(CompletionContent::Text(text));
                Vec::new()
            }
            BlockAccumulator::Tool {
                id,
                name,
                partial_json,
            } => {
                let input = if partial_json.trim().is_empty() {
                    Value::Object(Map::new())
                } else {
                    match serde_json::from_str(&partial_json) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                tool_name = %name,
                                payload_bytes = partial_json.len(),
                                "Anthropic tool input JSON failed to parse; falling back to empty object"
                            );
                            Value::Object(Map::new())
                        }
                    }
                };
                let tool_call = ToolInvocation {
                    id: Some(id),
                    name,
                    input,
                };
                let content =
                    CompletionContent::ToolCall(moa_core::types::completion::ToolCallContent {
                        invocation: tool_call.clone(),
                        provider_metadata: None,
                    });
                self.completed_content[payload.index] = Some(content.clone());
                vec![content]
            }
            BlockAccumulator::ServerTool { name, partial_json } => {
                let block = CompletionContent::ProviderToolResult {
                    tool_name: name.clone(),
                    summary: summarize_anthropic_server_tool_use(&name, &partial_json),
                };
                self.completed_content[payload.index] = Some(block.clone());
                vec![block]
            }
            BlockAccumulator::Ignored => Vec::new(),
        }
    }

    fn finish(mut self, started_at: Instant) -> CompletionResponse {
        for index in 0..self.blocks.len() {
            self.ensure_completed_capacity(index);
            match &self.blocks[index] {
                BlockAccumulator::Text(text) => {
                    if self.completed_content[index].is_none() {
                        self.completed_content[index] = Some(CompletionContent::Text(text.clone()));
                    }
                }
                BlockAccumulator::Tool {
                    id,
                    name,
                    partial_json,
                } => {
                    if self.completed_content[index].is_none() {
                        let input = if partial_json.trim().is_empty() {
                            Value::Object(Map::new())
                        } else {
                            match serde_json::from_str(partial_json) {
                                Ok(value) => value,
                                Err(error) => {
                                    tracing::warn!(
                                        %error,
                                        tool_name = %name,
                                        payload_bytes = partial_json.len(),
                                        "Anthropic tool input JSON failed to parse on finish; falling back to empty object"
                                    );
                                    Value::Object(Map::new())
                                }
                            }
                        };
                        self.completed_content[index] = Some(CompletionContent::ToolCall(
                            moa_core::types::completion::ToolCallContent {
                                invocation: ToolInvocation {
                                    id: Some(id.clone()),
                                    name: name.clone(),
                                    input,
                                },
                                provider_metadata: None,
                            },
                        ));
                    }
                }
                BlockAccumulator::ServerTool { name, partial_json } => {
                    if self.completed_content[index].is_none() {
                        self.completed_content[index] =
                            Some(CompletionContent::ProviderToolResult {
                                tool_name: name.clone(),
                                summary: summarize_anthropic_server_tool_use(name, partial_json),
                            });
                    }
                }
                BlockAccumulator::Ignored => {}
            }
        }

        let content: Vec<_> = self.completed_content.into_iter().flatten().collect();
        let text = content
            .iter()
            .filter_map(|block| match block {
                CompletionContent::Text(text) => Some(text.as_str()),
                CompletionContent::ToolCall(_) => None,
                CompletionContent::ProviderToolResult { .. } => None,
            })
            .collect::<String>();

        CompletionResponse {
            text,
            content,
            stop_reason: self.stop_reason,
            model: ModelId::new(self.model),
            usage: TokenUsage {
                input_tokens_uncached: self.input_tokens,
                input_tokens_cache_write: self.cache_creation_input_tokens,
                input_tokens_cache_read: self.cached_input_tokens,
                output_tokens: self.output_tokens,
            },
            duration_ms: started_at.elapsed().as_millis() as u64,
            thought_signature: None,
        }
    }

    fn ensure_capacity(&mut self, index: usize) {
        while self.blocks.len() <= index {
            self.blocks.push(BlockAccumulator::Ignored);
        }
    }

    fn ensure_completed_capacity(&mut self, index: usize) {
        while self.completed_content.len() <= index {
            self.completed_content.push(None);
        }
    }
}

#[derive(Debug, Clone)]
enum BlockAccumulator {
    Text(String),
    Tool {
        id: String,
        name: String,
        partial_json: String,
    },
    ServerTool {
        name: String,
        partial_json: String,
    },
    Ignored,
}

#[derive(Debug, Deserialize)]
struct MessageStartEvent {
    message: MessageEnvelope,
}

#[derive(Debug, Deserialize)]
struct MessageEnvelope {
    model: String,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
    #[serde(default)]
    cache_read_input_tokens: usize,
    #[serde(default)]
    cache_creation_input_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStartEvent {
    index: usize,
    content_block: ContentBlockStart,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockStart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ServerToolUse {
        #[serde(rename = "id")]
        _id: String,
        name: String,
    },
    WebSearchToolResult {
        #[serde(rename = "tool_use_id")]
        _tool_use_id: String,
        #[serde(default)]
        content: Vec<WebSearchResultContent>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDeltaEvent {
    index: usize,
    delta: ContentBlockDelta,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStopEvent {
    index: usize,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaEvent {
    delta: MessageDeltaPayload,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaPayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorEvent {
    error: StreamErrorPayload,
}

#[derive(Debug, Deserialize)]
struct StreamErrorPayload {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

fn initial_tool_input(input: Value) -> Result<String> {
    if input.is_null() {
        return Ok(String::new());
    }

    if let Value::Object(map) = &input
        && map.is_empty()
    {
        return Ok(String::new());
    }

    serde_json::to_string(&input).map_err(MoaError::from)
}
