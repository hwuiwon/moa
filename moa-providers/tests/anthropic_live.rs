use moa_core::{CompletionRequest, LLMProvider};
use moa_providers::AnthropicProvider;

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
