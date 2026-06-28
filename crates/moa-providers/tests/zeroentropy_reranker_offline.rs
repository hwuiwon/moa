//! Wiremock offline counterpart for ZeroEntropy reranker live coverage.

use moa_core::MoaError;
use moa_providers::{
    Reranker, ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyRerankLatency, ZeroEntropyReranker,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn zeroentropy_reranker_offline_prioritizes_relevant_retrieval_candidate() {
    // Pins: ZeroEntropy rerank calls the v1 rerank endpoint with model, query, documents, and top_n.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": ZEROENTROPY_DEFAULT_RERANK_MODEL,
            "query": "Where does MOA deploy the local validation service?",
            "top_n": 2,
            "latency": "fast"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 0, "relevance_score": 0.98 },
                { "index": 1, "relevance_score": 0.21 }
            ],
            "total_bytes": 123,
            "total_tokens": 17,
            "actual_latency_mode": "fast",
            "e2e_latency": 0.12,
            "inference_latency": 0.09
        })))
        .mount(&server)
        .await;

    let reranker = ZeroEntropyReranker::new("test-key")
        .expect("ZeroEntropy reranker should build")
        .with_endpoint(format!("{}/v1/models/rerank", server.uri()))
        .with_latency(Some(ZeroEntropyRerankLatency::Fast));
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
        .expect("wiremock ZeroEntropy rerank request should succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 0);
    assert!(hits[0].relevance_score >= hits[1].relevance_score);
}

#[tokio::test]
async fn zeroentropy_reranker_offline_empty_documents_skip_http_call() {
    // Pins: empty rerank batches return locally so providers that reject [] are normalized.
    let server = MockServer::start().await;
    let reranker = ZeroEntropyReranker::new("test-key")
        .expect("ZeroEntropy reranker should build")
        .with_endpoint(format!("{}/v1/models/rerank", server.uri()));

    let hits = reranker
        .rerank(ZEROENTROPY_DEFAULT_RERANK_MODEL, "query", &[], 4)
        .await
        .expect("empty rerank should succeed without HTTP");

    assert!(hits.is_empty());
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose request log");
    assert!(requests.is_empty());
}

#[tokio::test]
async fn zeroentropy_reranker_offline_http_error_surfaces_status() {
    // Pins: upstream ZeroEntropy failures preserve HTTP status for retry/failure classification.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let reranker = ZeroEntropyReranker::new("test-key")
        .expect("ZeroEntropy reranker should build")
        .with_endpoint(format!("{}/v1/models/rerank", server.uri()));
    let documents = vec!["hello".to_string()];

    let error = reranker
        .rerank(ZEROENTROPY_DEFAULT_RERANK_MODEL, "query", &documents, 1)
        .await
        .expect_err("401 should fail");

    assert!(
        matches!(error, MoaError::HttpStatus { status: 401, .. }),
        "expected HTTP 401, got {error:?}"
    );
}
