//! Wiremock offline counterpart for Cohere reranker live coverage.

use moa_providers::{CohereReranker, Reranker};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cohere_reranker_offline_prioritizes_relevant_retrieval_candidate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "rerank-v4.0-fast",
            "query": "Where does MOA deploy the local validation service?",
            "top_n": 2
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 0, "relevance_score": 0.98 },
                { "index": 1, "relevance_score": 0.21 }
            ]
        })))
        .mount(&server)
        .await;

    let reranker = CohereReranker::new("test-key")
        .expect("Cohere reranker should build")
        .with_endpoint(format!("{}/v2/rerank", server.uri()));
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
        .expect("wiremock Cohere rerank request should succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 0);
    assert!(hits[0].relevance_score >= hits[1].relevance_score);
}
