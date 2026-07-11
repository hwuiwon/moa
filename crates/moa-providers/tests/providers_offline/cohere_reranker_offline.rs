//! Wiremock offline counterpart for Cohere reranker live coverage.

use moa_core::error::MoaError;
use moa_providers::{CohereReranker, Reranker};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const QUERY: &str = "Where does MOA deploy the local validation service?";

fn documents() -> Vec<String> {
    vec![
        "MOA deploys its local validation service to railway.".to_string(),
        "MOA stores memory facts in PostgreSQL tables with RLS.".to_string(),
        "The hosted API surfaces status output and approval prompts.".to_string(),
    ]
}

#[tokio::test]
async fn cohere_reranker_offline_maps_out_of_order_hits_back_to_documents_and_drops_oob() {
    // Pins: rerank results arrive in relevance order (not input order) and may
    // include an out-of-range index; the provider maps each hit index back to
    // the supplied document and filters indices >= documents.len().
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "rerank-v4.0-fast",
            "query": QUERY,
            "top_n": 3
        })))
        // Non-natural order: index 2 first, then index 0, plus an out-of-range
        // index 9 that must be filtered out (only 3 documents were supplied).
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 2, "relevance_score": 0.91 },
                { "index": 0, "relevance_score": 0.88 },
                { "index": 9, "relevance_score": 0.42 }
            ]
        })))
        .mount(&server)
        .await;

    let reranker = CohereReranker::new("test-key")
        .expect("Cohere reranker should build")
        .with_endpoint(format!("{}/v2/rerank", server.uri()));
    let documents = documents();

    let hits = reranker
        .rerank("rerank-v4.0-fast", QUERY, &documents, 3)
        .await
        .expect("wiremock Cohere rerank request should succeed");

    // The out-of-range hit is dropped; the remaining two keep relevance order.
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 2);
    assert_eq!(hits[1].index, 0);
    assert!(hits.iter().all(|hit| hit.index < documents.len()));
    // Each surviving hit maps back to the right document text.
    assert_eq!(
        documents[hits[0].index],
        "The hosted API surfaces status output and approval prompts."
    );
    assert_eq!(
        documents[hits[1].index],
        "MOA deploys its local validation service to railway."
    );
}

#[tokio::test]
async fn cohere_reranker_offline_empty_documents_skip_http_call() {
    // Pins: empty rerank batches return locally so providers that reject [] are normalized.
    let server = MockServer::start().await;
    let reranker = CohereReranker::new("test-key")
        .expect("Cohere reranker should build")
        .with_endpoint(format!("{}/v2/rerank", server.uri()));

    let hits = reranker
        .rerank("rerank-v4.0-fast", "query", &[], 4)
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
async fn cohere_reranker_offline_http_error_surfaces_status() {
    // Pins: upstream Cohere failures preserve HTTP status for retry/failure classification.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let reranker = CohereReranker::new("test-key")
        .expect("Cohere reranker should build")
        .with_endpoint(format!("{}/v2/rerank", server.uri()));
    let documents = vec!["hello".to_string()];

    let error = reranker
        .rerank("rerank-v4.0-fast", "query", &documents, 1)
        .await
        .expect_err("401 should fail");

    assert!(
        matches!(error, MoaError::HttpStatus { status: 401, .. }),
        "expected HTTP 401, got {error:?}"
    );
}
