// Live counterpart: see zeroentropy_reranker_offline.rs for the wiremock version that runs in PR CI.

//! Live ZeroEntropy rerank coverage.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_ZEROENTROPY_TESTS=1` because they call a billed external API.

use moa_providers::{Reranker, ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyReranker};

fn live_zeroentropy_requested() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_ZEROENTROPY_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn live_zeroentropy_key() -> Option<String> {
    if !live_zeroentropy_requested() {
        return None;
    }

    let api_key = std::env::var("MOA_ZEROENTROPY_API_KEY")
        .expect("MOA_ZEROENTROPY_API_KEY is required when MOA_RUN_LIVE_ZEROENTROPY_TESTS=1");
    Some(api_key)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_ZEROENTROPY_TESTS=1 and MOA_ZEROENTROPY_API_KEY"]
async fn zeroentropy_zerank_2_prioritizes_relevant_retrieval_candidate() {
    let Some(api_key) = live_zeroentropy_key() else {
        return;
    };
    let reranker = ZeroEntropyReranker::new(api_key).expect("ZeroEntropy reranker should build");
    let documents = vec![
        "MOA deploys its local validation service to fly.io.".to_string(),
        "MOA stores memory facts in PostgreSQL tables with RLS.".to_string(),
        "The hosted API surfaces status output and approval prompts.".to_string(),
    ];

    let hits = reranker
        .rerank(
            ZEROENTROPY_DEFAULT_RERANK_MODEL,
            "Where does MOA deploy the local validation service?",
            &documents,
            2,
        )
        .await
        .expect("ZeroEntropy rerank live request should succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 0);
    assert!(hits[0].relevance_score >= hits[1].relevance_score);
}
