use std::time::Duration;

use moa_core::{CompletionContent, CompletionRequest, LLMProvider};
use moa_providers::AnthropicProvider;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn anthropic_live_completion_returns_expected_answer() {
    let provider = AnthropicProvider::from_env("claude-sonnet-4-6").unwrap();
    let response = provider
        .complete(CompletionRequest::simple(
            "What is 2+2? Respond with just the answer.",
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert!(response.text.contains('4'));
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn anthropic_live_web_search_emits_provider_tool_result() {
    let provider = AnthropicProvider::from_env("claude-sonnet-4-6").unwrap();
    let response = timeout(
        Duration::from_secs(90),
        provider.complete(CompletionRequest::simple(
            "Use web search to find one current news headline from today and cite the source in one short sentence.",
        )),
    )
    .await
    .unwrap()
    .unwrap()
    .collect()
    .await
    .unwrap();

    assert!(
        response
            .content
            .iter()
            .any(|block| matches!(block, CompletionContent::ProviderToolResult { tool_name, .. } if tool_name == "web_search")),
        "expected provider-native web search activity, got: {:?}",
        response.content
    );
    assert!(
        !response.text.trim().is_empty(),
        "expected a non-empty response after web search"
    );
}
