use std::collections::HashMap;
use std::time::Duration;

use moa_core::{CompletionContent, CompletionRequest, ContextMessage, LLMProvider};
use moa_providers::{AnthropicProvider, GeminiProvider, OpenAIProvider};
use serde_json::json;
use tokio::time::timeout;

enum LiveProvider {
    OpenAi(Box<OpenAIProvider>),
    Anthropic(Box<AnthropicProvider>),
    Google(Box<GeminiProvider>),
}

impl LiveProvider {
    fn label(&self) -> &'static str {
        match self {
            Self::OpenAi(_) => "openai",
            Self::Anthropic(_) => "anthropic",
            Self::Google(_) => "google",
        }
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::Result<moa_core::CompletionStream> {
        match self {
            Self::OpenAi(provider) => provider.complete(request).await,
            Self::Anthropic(provider) => provider.complete(request).await,
            Self::Google(provider) => provider.complete(request).await,
        }
    }
}

fn looks_like_four_answer(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    normalized.contains('4') || normalized.contains("four")
}

fn available_live_providers() -> Vec<LiveProvider> {
    let mut providers = Vec::new();
    if let Ok(provider) = OpenAIProvider::from_env("gpt-5.4") {
        providers.push(LiveProvider::OpenAi(Box::new(provider)));
    }
    if let Ok(provider) = AnthropicProvider::from_env("claude-sonnet-4-6") {
        providers.push(LiveProvider::Anthropic(Box::new(provider)));
    }
    if let Ok(provider) = GeminiProvider::from_env("gemini-3.1-pro-preview") {
        providers.push(LiveProvider::Google(Box::new(provider)));
    }
    providers
}

fn emit_token_tool() -> serde_json::Value {
    json!({
        "name": "emit_token",
        "description": "Echoes a validation token so the caller can confirm tool use.",
        "input_schema": {
            "type": "object",
            "properties": {
                "token": {
                    "type": "string",
                    "description": "Validation token to echo back."
                }
            },
            "required": ["token"],
            "additionalProperties": false
        }
    })
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_answer_simple_prompt_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let response = provider
            .complete(CompletionRequest::simple(
                "What is 2+2? Respond with just the answer.",
            ))
            .await
            .unwrap_or_else(|error| {
                panic!("{} simple completion failed: {error}", provider.label())
            })
            .collect()
            .await
            .unwrap_or_else(|error| {
                panic!("{} stream collection failed: {error}", provider.label())
            });

        assert!(
            looks_like_four_answer(&response.text),
            "{} response did not look like a 4-answer: {:?}",
            provider.label(),
            response.text
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_emit_tool_calls_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let token = format!("LIVE-TOOL-{}", provider.label().to_uppercase());
        let mut metadata = HashMap::new();
        metadata.insert("suite".to_string(), json!("live-provider-matrix"));
        let response = provider
            .complete(CompletionRequest {
                model: None,
                messages: vec![moa_core::ContextMessage::user(format!(
                    "You must call the emit_token tool exactly once with token \"{token}\". \
                     Do not answer in plain text before the tool call."
                ))],
                tools: vec![emit_token_tool()],
                max_output_tokens: Some(256),
                temperature: None,
                cache_breakpoints: Vec::new(),
                cache_controls: Vec::new(),
                metadata,
            })
            .await
            .unwrap_or_else(|error| {
                panic!("{} tool-call request failed: {error}", provider.label())
            })
            .collect()
            .await
            .unwrap_or_else(|error| {
                panic!("{} tool-call stream failed: {error}", provider.label())
            });

        let tool_call = response.content.iter().find_map(|content| match content {
            CompletionContent::ToolCall(call) => Some(&call.invocation),
            CompletionContent::Text(_) => None,
            CompletionContent::ProviderToolResult { .. } => None,
        });
        let Some(tool_call) = tool_call else {
            panic!(
                "{} did not emit a tool call. Response content: {:?}",
                provider.label(),
                response.content
            );
        };

        assert_eq!(tool_call.name, "emit_token");
        assert_eq!(
            tool_call
                .input
                .get("token")
                .and_then(|value| value.as_str()),
            Some(token.as_str())
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_can_use_native_web_search_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let response = timeout(
            Duration::from_secs(90),
            provider.complete(CompletionRequest::simple(
                "Use web search to find one current news headline from today and cite the source in one short sentence.",
            )),
        )
        .await
        .unwrap_or_else(|_| panic!("{} web-search request timed out", provider.label()))
        .unwrap_or_else(|error| panic!("{} web-search request failed: {error}", provider.label()))
        .collect()
        .await
        .unwrap_or_else(|error| panic!("{} web-search stream failed: {error}", provider.label()));

        let has_provider_tool_result = response.content.iter().any(|content| {
            matches!(content, CompletionContent::ProviderToolResult { tool_name, .. } if tool_name == "web_search")
        });
        let has_citation = response.text.contains("http://")
            || response.text.contains("https://")
            || response.text.contains('[');

        assert!(
            has_provider_tool_result || has_citation,
            "{} did not show evidence of grounded web search: {:?}",
            provider.label(),
            response
        );
        assert!(
            !response.text.trim().is_empty(),
            "{} returned an empty response after web search",
            provider.label()
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_obey_system_prompt_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    let marker = "[E2E-SYS-MARKER-9421]";

    for provider in providers {
        let response = provider
            .complete(CompletionRequest {
                model: None,
                messages: vec![
                    ContextMessage::system(format!(
                        "You must end every reply with exactly this literal marker, including brackets: {marker}"
                    )),
                    ContextMessage::user("Say hello in one short sentence."),
                ],
                tools: Vec::new(),
                max_output_tokens: Some(64),
                temperature: None,
                cache_breakpoints: Vec::new(),
                cache_controls: Vec::new(),
                metadata: HashMap::new(),
            })
            .await
            .unwrap_or_else(|e| {
                panic!("{} system-prompt request failed: {e}", provider.label())
            })
            .collect()
            .await
            .unwrap_or_else(|e| {
                panic!("{} system-prompt stream failed: {e}", provider.label())
            });

        assert!(
            response.text.contains(marker),
            "{} did not honor system prompt. Response: {:?}",
            provider.label(),
            response.text
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_stream_incrementally_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let mut stream = provider
            .complete(CompletionRequest::simple(
                "Count from 1 to 5 on a single line, comma-separated. No other words.",
            ))
            .await
            .unwrap_or_else(|e| panic!("{} streaming request failed: {e}", provider.label()));

        let mut streamed_text = String::new();
        let mut text_chunks = 0usize;
        while let Some(block) = stream.next().await {
            let block =
                block.unwrap_or_else(|e| panic!("{} streamed chunk error: {e}", provider.label()));
            if let CompletionContent::Text(t) = block {
                streamed_text.push_str(&t);
                text_chunks += 1;
            }
        }

        let response = stream
            .into_response()
            .await
            .unwrap_or_else(|e| panic!("{} finalization failed: {e}", provider.label()));

        assert!(
            text_chunks > 0,
            "{} produced zero text chunks during streaming",
            provider.label()
        );
        assert_eq!(
            streamed_text.trim(),
            response.text.trim(),
            "{} streamed text does not match aggregated response (chunks={text_chunks})",
            provider.label()
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_report_token_usage_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let response = provider
            .complete(CompletionRequest::simple(
                "Name three primary colors as a comma-separated list.",
            ))
            .await
            .unwrap_or_else(|e| panic!("{} usage request failed: {e}", provider.label()))
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{} usage stream failed: {e}", provider.label()));

        let usage = response.token_usage();
        assert!(
            usage.total_input_tokens() > 0,
            "{} reported zero input tokens: {:?}",
            provider.label(),
            usage
        );
        assert!(
            usage.output_tokens > 0,
            "{} reported zero output tokens: {:?}",
            provider.label(),
            usage
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_truncate_at_max_output_tokens_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let response = provider
            .complete(CompletionRequest {
                model: None,
                messages: vec![ContextMessage::user(
                    "Describe the history of the Roman Empire in full detail.",
                )],
                tools: Vec::new(),
                // OpenAI's Responses API rejects max_output_tokens < 16, so use the shared floor.
                max_output_tokens: Some(16),
                temperature: None,
                cache_breakpoints: Vec::new(),
                cache_controls: Vec::new(),
                metadata: HashMap::new(),
            })
            .await
            .unwrap_or_else(|e| panic!("{} max-tokens request failed: {e}", provider.label()))
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{} max-tokens stream failed: {e}", provider.label()));

        let word_count = response.text.split_whitespace().count();
        assert!(
            word_count <= 30,
            "{} ignored max_output_tokens=16 and produced {word_count} words: {:?}",
            provider.label(),
            response.text
        );
    }
}

#[tokio::test]
#[ignore = "manual live provider matrix test"]
async fn live_providers_preserve_unicode_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let response = provider
            .complete(CompletionRequest {
                model: None,
                messages: vec![ContextMessage::user(
                    "Echo these three tokens on one line, separated by a single space, with no quotes or extra words: 🦀 你好 مرحبا",
                )],
                tools: Vec::new(),
                max_output_tokens: Some(64),
                temperature: None,
                cache_breakpoints: Vec::new(),
                cache_controls: Vec::new(),
                metadata: HashMap::new(),
            })
            .await
            .unwrap_or_else(|e| panic!("{} unicode request failed: {e}", provider.label()))
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{} unicode stream failed: {e}", provider.label()));

        assert!(
            response.text.contains('🦀'),
            "{} dropped the 🦀 codepoint: {:?}",
            provider.label(),
            response.text
        );
        assert!(
            response.text.contains("你好"),
            "{} dropped the CJK segment: {:?}",
            provider.label(),
            response.text
        );
        assert!(
            response.text.contains("مرحبا"),
            "{} dropped the Arabic segment: {:?}",
            provider.label(),
            response.text
        );
    }
}
