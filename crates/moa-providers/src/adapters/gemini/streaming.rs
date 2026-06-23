//! Gemini SSE stream normalization.

use super::response::{
    GeminiGenerateContentResponse, GeminiUsageMetadata, finish_reason_to_stop_reason,
    token_usage_from_gemini_usage,
};
use super::tools::normalize_tool_args;
use super::*;
use crate::core::provider_tools::{web_search_completed_block, web_search_started_block};

pub(super) async fn consume_sse_events<S>(
    stream: S,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    span_recorder: &mut LLMSpanRecorder,
) -> Result<CompletionResponse>
where
    S: Stream<
        Item = std::result::Result<SseEvent, eventsource_stream::EventStreamError<reqwest::Error>>,
    >,
{
    let mut state = GeminiStreamState::new(fallback_model);
    pin_mut!(stream);

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                return Err(MoaError::StreamError(format!(
                    "failed to read Gemini SSE event: {error}"
                )));
            }
        };

        for block in state.apply_event(&event)? {
            span_recorder.observe_block(&block);
            if tx.send(Ok(block)).await.is_err() {
                tracing::debug!("completion stream receiver dropped before the response finished");
                return Ok(state.finish(started_at));
            }
        }
    }

    if !state.saw_terminal_finish {
        return Err(MoaError::StreamError(
            "Gemini stream ended before the provider returned a terminal finishReason".to_string(),
        ));
    }
    Ok(state.finish(started_at))
}

#[derive(Debug)]
struct GeminiStreamState {
    model: String,
    text: String,
    content: Vec<CompletionContent>,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    stop_reason: StopReason,
    thought_signature: Option<String>,
    search_started_emitted: bool,
    search_completed_emitted: bool,
    saw_terminal_finish: bool,
    last_raw_response: Option<GeminiGenerateContentResponse>,
}

impl GeminiStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            text: String::new(),
            content: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            stop_reason: StopReason::EndTurn,
            thought_signature: None,
            search_started_emitted: false,
            search_completed_emitted: false,
            saw_terminal_finish: false,
            last_raw_response: None,
        }
    }

    fn apply_event(&mut self, event: &SseEvent) -> Result<Vec<CompletionContent>> {
        let response: GeminiGenerateContentResponse = parse_sse_json(event)?;
        if let Some(model_version) = response.model_version.clone()
            && !model_version.is_empty()
        {
            self.model = model_version;
        }
        if let Some(ref usage) = response.usage_metadata {
            self.input_tokens = usage.prompt_token_count.unwrap_or(self.input_tokens);
            self.output_tokens = usage.candidates_token_count.unwrap_or(self.output_tokens);
            self.cached_input_tokens = usage
                .cached_content_token_count
                .unwrap_or(self.cached_input_tokens);
        }
        self.last_raw_response = Some(response);
        let candidates = self
            .last_raw_response
            .as_ref()
            .ok_or_else(|| {
                MoaError::ProviderError(
                    "Gemini stream parser lost the last raw response".to_string(),
                )
            })?
            .candidates
            .clone();

        let mut emitted = Vec::new();
        for candidate in candidates {
            if candidate.grounding_metadata.is_some() && !self.search_started_emitted {
                self.search_started_emitted = true;
                let block = web_search_started_block();
                self.content.push(block.clone());
                emitted.push(block);
            }

            if let Some(content) = candidate.content {
                for part in content.parts {
                    if let Some(function_call) = part.function_call {
                        let call = CompletionContent::ToolCall(ToolCallContent {
                            invocation: ToolInvocation {
                                id: function_call.id.clone(),
                                name: function_call.name,
                                input: normalize_tool_args(&function_call.args),
                            },
                            provider_metadata: part.thought_signature.clone().map(
                                |thought_signature| ProviderToolCallMetadata::Gemini {
                                    thought_signature,
                                },
                            ),
                        });
                        self.content.push(call.clone());
                        emitted.push(call);
                        continue;
                    }

                    if let Some(text) = part.text
                        && !text.is_empty()
                    {
                        self.text.push_str(&text);
                        let block = CompletionContent::Text(text);
                        self.content.push(block.clone());
                        emitted.push(block);
                    }

                    if part.thought_signature.is_some() {
                        self.thought_signature = part.thought_signature;
                    }
                }
            }

            if let Some(finish_reason) = candidate.finish_reason.as_deref() {
                self.saw_terminal_finish = true;
                self.stop_reason = finish_reason_to_stop_reason(finish_reason);
                if candidate.grounding_metadata.is_some() && !self.search_completed_emitted {
                    self.search_completed_emitted = true;
                    let block = web_search_completed_block();
                    self.content.push(block.clone());
                    emitted.push(block);
                }
            }
        }

        Ok(emitted)
    }

    fn finish(mut self, started_at: Instant) -> CompletionResponse {
        if self
            .content
            .iter()
            .any(|entry| matches!(entry, CompletionContent::ToolCall(_)))
        {
            self.stop_reason = StopReason::ToolUse;
        }

        if self.content.is_empty() && !self.text.is_empty() {
            self.content
                .push(CompletionContent::Text(self.text.clone()));
        }

        CompletionResponse {
            text: self.text,
            content: self.content,
            stop_reason: self.stop_reason,
            model: ModelId::new(self.model),
            usage: token_usage_from_gemini_usage(&GeminiUsageMetadata {
                prompt_token_count: Some(self.input_tokens),
                candidates_token_count: Some(self.output_tokens),
                cached_content_token_count: Some(self.cached_input_tokens),
            }),
            duration_ms: started_at.elapsed().as_millis() as u64,
            thought_signature: self.thought_signature,
        }
    }
}
