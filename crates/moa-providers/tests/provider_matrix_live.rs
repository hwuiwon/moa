//! Ignored live matrix tests for real chat provider behavior.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use moa_core::{
    traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::JsonResponseFormat,
    types::context::ContextMessage,
};
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
    ) -> moa_core::error::Result<moa_core::types::completion::CompletionStream> {
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

/// Returns `true` when a raw flag value is a common truthy token (`1`, `true`,
/// `yes`, or `on`), case-insensitively and ignoring surrounding whitespace, so
/// a `.env` written as `MOA_RUN_LIVE_PROVIDER_TESTS=true` enables the live lane.
fn flag_value_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Returns `true` when the named env var is set to a truthy value (see
/// [`flag_value_enabled`]); unset or any other value is treated as disabled.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| flag_value_enabled(&value))
        .unwrap_or(false)
}

fn live_provider_tests_enabled() -> bool {
    env_flag_enabled("MOA_RUN_LIVE_PROVIDER_TESTS")
}

#[test]
fn truthy_flag_values_enable_the_live_lane() {
    // Pins: live gating accepts the truthy spellings a developer's `.env` uses,
    // and rejects falsey/unset values so billed lanes never run by accident.
    for value in ["1", "true", "TRUE", "Yes", " on "] {
        assert!(
            flag_value_enabled(value),
            "{value:?} should enable live tests"
        );
    }
    for value in ["0", "false", "", "  ", "off"] {
        assert!(
            !flag_value_enabled(value),
            "{value:?} should not enable live tests"
        );
    }
    // An unset env var resolves to disabled without mutating shared process state.
    assert!(!env_flag_enabled(
        "MOA_RUN_LIVE_PROVIDER_TESTS_UNSET_PROBE_0a7e3750"
    ));
}

fn available_live_providers() -> Vec<LiveProvider> {
    if !live_provider_tests_enabled() {
        return Vec::new();
    }

    let mut providers = Vec::new();
    if let Ok(provider) = OpenAIProvider::from_env("gpt-5.4") {
        providers.push(LiveProvider::OpenAi(Box::new(provider)));
    }
    if let Ok(provider) = AnthropicProvider::from_env("claude-sonnet-4-6") {
        providers.push(LiveProvider::Anthropic(Box::new(provider)));
    }
    if let Ok(provider) = GeminiProvider::from_env(google_live_model()) {
        providers.push(LiveProvider::Google(Box::new(provider)));
    }
    assert!(
        !providers.is_empty(),
        "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires at least one provider credential: MOA_OPENAI_API_KEY, MOA_ANTHROPIC_API_KEY, or MOA_GOOGLE_API_KEY"
    );
    providers
}

fn google_live_model() -> String {
    std::env::var("GOOGLE_MODEL").unwrap_or_else(|_| "gemini-3-flash-preview".to_string())
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

fn normalized_response_text(response: &moa_core::types::completion::CompletionResponse) -> String {
    if !response.text.trim().is_empty() {
        return response.text.clone();
    }

    response
        .content
        .iter()
        .filter_map(|content| match content {
            CompletionContent::Text(text) => Some(text.as_str()),
            CompletionContent::ToolCall(_) | CompletionContent::ProviderToolResult { .. } => None,
        })
        .collect()
}

async fn complete_until(
    provider: &LiveProvider,
    request: CompletionRequest,
    attempts: usize,
    mut predicate: impl FnMut(&str) -> bool,
) -> moa_core::types::completion::CompletionResponse {
    let mut last_response = None;
    for attempt in 0..attempts.max(1) {
        let response = provider
            .complete(request.clone())
            .await
            .unwrap_or_else(|error| panic!("{} live request failed: {error}", provider.label()))
            .collect()
            .await
            .unwrap_or_else(|error| panic!("{} live stream failed: {error}", provider.label()));
        let text = normalized_response_text(&response);
        if predicate(&text) || attempt + 1 == attempts.max(1) {
            return response;
        }
        last_response = Some(response);
    }

    last_response.expect("complete_until should always return a response")
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and MOA_OPENAI_API_KEY"]
async fn live_openai_structured_output_returns_direct_response() {
    if !live_provider_tests_enabled() {
        return;
    }

    let provider = OpenAIProvider::from_env("gpt-5.4-nano")
        .expect("MOA_RUN_LIVE_PROVIDER_TESTS=1 requires MOA_OPENAI_API_KEY for this OpenAI test");
    let mut metadata = HashMap::new();
    metadata.insert("_moa.openai.reasoning_effort".to_string(), json!("none"));
    let request = CompletionRequest {
        model: None,
        messages: vec![ContextMessage::user(
            "Return a JSON object with answer set to four.",
        )],
        tools: Vec::new(),
        max_output_tokens: Some(64),
        temperature: Some(0.0),
        response_format: Some(JsonResponseFormat::strict_json_schema(
            "answer_payload",
            "Small structured answer.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "answer": { "type": "string" }
                },
                "required": ["answer"]
            }),
        )),
        native_web_search: Default::default(),
        metadata,
    };

    let started = Instant::now();
    let response = timeout(Duration::from_secs(10), async {
        provider.complete(request).await?.collect().await
    })
    .await
    .expect("OpenAI structured output request should finish within 10 seconds")
    .expect("OpenAI structured output request should succeed");

    let payload: serde_json::Value = serde_json::from_str(&response.text)
        .expect("strict structured response must be valid JSON");
    let answer = payload
        .get("answer")
        .and_then(serde_json::Value::as_str)
        .expect("strict structured response must contain a string answer");
    assert!(
        looks_like_four_answer(answer),
        "unexpected structured answer in response: {:?}",
        response.text
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "structured output took too long: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
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
                messages: vec![moa_core::types::context::ContextMessage::user(format!(
                    "You must call the emit_token tool exactly once with token \"{token}\". \
                     Do not answer in plain text before the tool call."
                ))],
                tools: vec![emit_token_tool()],
                max_output_tokens: Some(256),
                temperature: None,
                response_format: None,
                native_web_search: Default::default(),
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
async fn live_providers_obey_system_prompt_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    let marker = "[E2E-SYS-MARKER-9421]";

    for provider in providers {
        let response = complete_until(
            &provider,
            CompletionRequest {
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
                response_format: None,
                native_web_search: Default::default(),
                metadata: HashMap::new(),
            },
            3,
            |text| text.contains(marker),
        )
        .await;
        let text = normalized_response_text(&response);

        assert!(
            text.contains(marker),
            "{} did not honor system prompt. Response: {:?}",
            provider.label(),
            text
        );
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
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
                response_format: None,
                native_web_search: Default::default(),
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
async fn live_providers_preserve_unicode_across_available_keys() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let response = complete_until(
            &provider,
            CompletionRequest {
                model: None,
                messages: vec![ContextMessage::user(
                    "Echo these three tokens on one line, separated by a single space, with no quotes or extra words: 🦀 你好 مرحبا",
                )],
                tools: Vec::new(),
                max_output_tokens: Some(64),
                temperature: None,
                response_format: None,
                native_web_search: Default::default(),
                metadata: HashMap::new(),
            },
            3,
            |text| text.contains('🦀') && text.contains("你好") && text.contains("مرحبا"),
        )
        .await;
        let text = normalized_response_text(&response);

        assert!(
            text.contains('🦀'),
            "{} dropped the 🦀 codepoint: {:?}",
            provider.label(),
            text
        );
        assert!(
            text.contains("你好"),
            "{} dropped the CJK segment: {:?}",
            provider.label(),
            text
        );
        assert!(
            text.contains("مرحبا"),
            "{} dropped the Arabic segment: {:?}",
            provider.label(),
            text
        );
    }
}
