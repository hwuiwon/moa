use std::collections::HashMap;

use moa_core::{CompletionContent, CompletionRequest, LLMProvider};
use moa_providers::{AnthropicProvider, OpenAIProvider, OpenRouterProvider};
use serde_json::json;

enum LiveProvider {
    OpenAi(Box<OpenAIProvider>),
    Anthropic(Box<AnthropicProvider>),
    OpenRouter(Box<OpenRouterProvider>),
}

impl LiveProvider {
    fn label(&self) -> &'static str {
        match self {
            Self::OpenAi(_) => "openai",
            Self::Anthropic(_) => "anthropic",
            Self::OpenRouter(_) => "openrouter",
        }
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::Result<moa_core::CompletionStream> {
        match self {
            Self::OpenAi(provider) => provider.complete(request).await,
            Self::Anthropic(provider) => provider.complete(request).await,
            Self::OpenRouter(provider) => provider.complete(request).await,
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
    if let Ok(provider) = OpenRouterProvider::from_env("openai/gpt-5.4") {
        providers.push(LiveProvider::OpenRouter(Box::new(provider)));
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
            CompletionContent::ToolCall(invocation) => Some(invocation),
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
