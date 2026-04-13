use std::collections::HashMap;
use std::time::Duration;

use moa_core::{CompletionContent, CompletionRequest, LLMProvider};
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

fn available_live_providers() -> Vec<LiveProvider> {
    let mut providers = Vec::new();
    if let Ok(provider) = OpenAIProvider::from_env("gpt-5.4") {
        providers.push(LiveProvider::OpenAi(Box::new(provider)));
    }
    if let Ok(provider) = AnthropicProvider::from_env("claude-sonnet-4-6") {
        providers.push(LiveProvider::Anthropic(Box::new(provider)));
    }
    if let Ok(provider) = GeminiProvider::from_env("gemini-2.5-flash") {
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
            response.text.contains('4'),
            "{} response did not contain 4: {:?}",
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
