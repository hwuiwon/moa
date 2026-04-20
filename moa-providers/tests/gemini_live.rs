use std::time::Duration;

use moa_core::{CompletionContent, CompletionRequest, LLMProvider};
use moa_providers::GeminiProvider;
use tokio::time::timeout;

fn gemini_live_model() -> String {
    std::env::var("GOOGLE_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string())
}

fn looks_like_four_answer(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    normalized.contains('4') || normalized.contains("four")
}

#[tokio::test]
#[ignore = "requires GOOGLE_API_KEY"]
async fn gemini_live_completion_returns_expected_answer() {
    let provider = GeminiProvider::from_env(gemini_live_model()).unwrap();
    let response = provider
        .complete(CompletionRequest::simple(
            "What is 2+2? Respond with just the answer.",
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert!(
        looks_like_four_answer(&response.text),
        "unexpected arithmetic response: {:?}",
        response.text
    );
}

#[tokio::test]
#[ignore = "requires GOOGLE_API_KEY"]
async fn gemini_live_web_search_returns_current_information() {
    let provider = GeminiProvider::from_env(gemini_live_model()).unwrap();
    let mut last_response = String::new();

    for _ in 0..3 {
        let response = timeout(
            Duration::from_secs(90),
            provider.complete(CompletionRequest {
                model: None,
                messages: vec![moa_core::ContextMessage::user(
                    "Use web search to find one current news headline from today and cite the source in one short sentence.",
                )],
                tools: Vec::new(),
                max_output_tokens: Some(128),
                temperature: Some(0.0),
                cache_breakpoints: Vec::new(),
                cache_controls: Vec::new(),
                metadata: Default::default(),
            })
            .await
            .unwrap()
            .collect(),
        )
        .await
        .unwrap()
        .unwrap();

        last_response = response.text.clone();
        let has_provider_tool_result = response.content.iter().any(|block| {
            matches!(
                block,
                CompletionContent::ProviderToolResult { tool_name, .. } if tool_name == "web_search"
            )
        });
        let has_citation = response.text.contains("http://")
            || response.text.contains("https://")
            || response.text.contains('[');

        if has_provider_tool_result || has_citation {
            return;
        }
    }

    panic!(
        "expected grounded web output after retries, got: {}",
        last_response
    );
}
