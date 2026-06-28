// Live counterpart: see cohere_reranker_offline.rs for the wiremock version that runs in PR CI.

//! Live Cohere Rerank coverage for the hybrid retriever client.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_COHERE_TESTS=1` because they call a billed external API.

use moa_providers::{CohereReranker, Reranker};

fn live_cohere_requested() -> bool {
    matches!(
        std::env::var("MOA_RUN_LIVE_COHERE_TESTS").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn live_cohere_key() -> Option<String> {
    if !live_cohere_requested() {
        return None;
    }

    let api_key = std::env::var("MOA_COHERE_API_KEY")
        .expect("MOA_COHERE_API_KEY is required when MOA_RUN_LIVE_COHERE_TESTS=1");
    Some(api_key)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_COHERE_TESTS=1 and MOA_COHERE_API_KEY"]
async fn cohere_rerank_v4_fast_prioritizes_relevant_retrieval_candidate() {
    let Some(api_key) = live_cohere_key() else {
        return;
    };
    let reranker = CohereReranker::new(api_key).expect("Cohere reranker should build");
    let documents = vec![
        "MOA deploys its local validation service to fly.io.".to_string(),
        "MOA stores memory facts in PostgreSQL tables with RLS.".to_string(),
        "The hosted API surfaces status output and approval prompts.".to_string(),
    ];

    let hits = reranker
        .rerank(
            "rerank-v4.0-fast",
            "Where does MOA deploy the local validation service?",
            &documents,
            2,
        )
        .await
        .expect("Cohere Rerank v4 live request should succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 0);
    assert!(hits[0].relevance_score >= hits[1].relevance_score);
}
