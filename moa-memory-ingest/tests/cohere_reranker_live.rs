//! Live Cohere Rerank coverage for the contradiction detector client.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_COHERE_TESTS=1` because they call a billed external API.

use moa_memory_ingest::contradiction::{CohereReranker, Reranker};
use secrecy::SecretString;

fn live_cohere_requested() -> bool {
    matches!(
        std::env::var("MOA_RUN_LIVE_COHERE_TESTS").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn live_cohere_key() -> Option<SecretString> {
    if !live_cohere_requested() {
        return None;
    }

    let api_key = std::env::var("COHERE_API_KEY")
        .or_else(|_| std::env::var("MOA_COHERE_API_KEY"))
        .expect(
            "COHERE_API_KEY or MOA_COHERE_API_KEY is required when \
             MOA_RUN_LIVE_COHERE_TESTS=1",
        );
    Some(SecretString::from(api_key))
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_COHERE_TESTS=1 and COHERE_API_KEY or MOA_COHERE_API_KEY"]
async fn cohere_rerank_v4_fast_prioritizes_contradiction_candidate() {
    let Some(api_key) = live_cohere_key() else {
        return;
    };
    let reranker = CohereReranker::new(api_key);
    let documents = vec![
        "The team deploys the auth service to fly.io.".to_string(),
        "The team stores audit records for six years.".to_string(),
        "The desktop app uses compact status indicators.".to_string(),
    ];

    let hits = reranker
        .rerank(
            "rerank-v4.0-fast",
            "The team deploys the auth service to AWS.",
            &documents,
            2,
        )
        .await
        .expect("Cohere Rerank v4 live request should succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 0);
    assert!(hits[0].relevance_score >= hits[1].relevance_score);
}
