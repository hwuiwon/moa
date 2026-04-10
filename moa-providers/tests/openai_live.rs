use moa_core::{CompletionRequest, LLMProvider};
use moa_providers::OpenAIProvider;

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn openai_live_completion_returns_expected_answer() {
    let provider = OpenAIProvider::from_env("gpt-5.4").unwrap();
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
