//! Wiremock offline counterpart for ZeroEntropy reranker live coverage.

use moa_core::MoaError;
use moa_providers::{
    Reranker, ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyRerankLatency, ZeroEntropyReranker,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn zeroentropy_reranker_offline_maps_out_of_order_hits_back_to_documents_and_drops_oob() {
    // Pins: ZeroEntropy rerank returns hits in relevance order (not input order)
    // and may include an out-of-range index; the provider maps each hit index
    // back to the supplied document and filters indices >= documents.len().
    let server = MockServer::start().await;
    let query = "Where does MOA deploy the local validation service?";
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": ZEROENTROPY_DEFAULT_RERANK_MODEL,
            "query": query,
            "top_n": 3,
            "latency": "fast"
        })))
        // Non-natural order: index 2 first, then index 0, plus an out-of-range
        // index 7 that must be filtered out (only 3 documents were supplied).
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 2, "relevance_score": 0.95 },
                { "index": 0, "relevance_score": 0.60 },
                { "index": 7, "relevance_score": 0.33 }
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
        .rerank(ZEROENTROPY_DEFAULT_RERANK_MODEL, query, &documents, 3)
        .await
        .expect("wiremock ZeroEntropy rerank request should succeed");

    // The out-of-range hit is dropped; the remaining two keep relevance order.
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 2);
    assert_eq!(hits[1].index, 0);
    assert!(hits.iter().all(|hit| hit.index < documents.len()));
    assert!(hits[0].relevance_score >= hits[1].relevance_score);
    // Each surviving hit maps back to the right document text.
    assert_eq!(
        documents[hits[0].index],
        "The hosted API surfaces status output and approval prompts."
    );
    assert_eq!(
        documents[hits[1].index],
        "MOA deploys its local validation service to fly.io."
    );
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
