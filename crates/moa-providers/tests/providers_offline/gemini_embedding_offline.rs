//! Deterministic Gemini embedder request-shape coverage.

use moa_core::traits::EmbeddingProvider;
use moa_providers::{EmbedderConstructionRole, GeminiEmbeddingEmbedder};
use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BATCH_PATH: &str = "/v1beta/models/gemini-embedding-2:batchEmbedContents";

#[tokio::test]
async fn gemini_v2_uses_prompt_prefix_and_snake_case_output_dimensionality() {
    // Pins: Gemini Embedding 2 asymmetric retrieval roles are represented as
    // prompt prefixes and batched through `:batchEmbedContents`, where each entry
    // carries its fully-qualified model id and snake_case output dimensionality.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(BATCH_PATH))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(batch_response(&[vec![0.5_f32; 128]])),
        )
        .mount(&server)
        .await;

    let embedder =
        GeminiEmbeddingEmbedder::new("test-key", 128, EmbedderConstructionRole::Retrieval)
            .expect("v2 embedder config should be valid")
            .with_endpoint(format!("{}/v1beta", server.uri()));

    let embeddings = embedder
        .embed(&["oauth".to_string()])
        .await
        .expect("mock v2 embedding request should succeed");

    assert_eq!(embeddings[0].len(), 128);
    let request = only_request(&server).await;
    let body: Value = serde_json::from_slice(&request.body)
        .expect("captured v2 request body should be valid JSON");
    let entry = &body["requests"][0];
    assert_eq!(entry["output_dimensionality"], 128);
    assert_eq!(entry["model"], "models/gemini-embedding-2");
    assert!(entry.get("taskType").is_none());
    assert_eq!(
        entry["content"]["parts"][0]["text"],
        "task: search result | query: oauth"
    );
}

#[tokio::test]
async fn gemini_v2_does_not_renormalize_server_output() {
    // Pins: provider adapters preserve Gemini's returned vector values.
    let server = MockServer::start().await;
    let mut values = vec![0.0_f32; 128];
    values[0] = 0.6;
    values[1] = 0.8;
    Mock::given(method("POST"))
        .and(path(BATCH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(batch_response(&[values])))
        .mount(&server)
        .await;

    let embedder =
        GeminiEmbeddingEmbedder::new("test-key", 128, EmbedderConstructionRole::Ingestion)
            .expect("v2 embedder config should be valid")
            .with_endpoint(format!("{}/v1beta", server.uri()));

    let embeddings = embedder
        .embed(&["oauth".to_string()])
        .await
        .expect("mock v2 embedding request should succeed");

    assert_eq!(embeddings[0][0], 0.6);
    assert_eq!(embeddings[0][1], 0.8);
    let request = only_request(&server).await;
    let body: Value = serde_json::from_slice(&request.body)
        .expect("captured v2 request body should be valid JSON");
    assert_eq!(
        body["requests"][0]["content"]["parts"][0]["text"],
        "title: none | text: oauth"
    );
}

#[tokio::test]
async fn gemini_v2_chunks_inputs_beyond_batch_limit_and_preserves_order() {
    // Pins: more than one batch worth of inputs is split into multiple
    // `:batchEmbedContents` calls whose results reassemble in input order.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(BATCH_PATH))
        .respond_with(MarkedBatchEmbeddings { dim: 128 })
        .mount(&server)
        .await;

    let embedder =
        GeminiEmbeddingEmbedder::new("test-key", 128, EmbedderConstructionRole::Retrieval)
            .expect("v2 embedder config should be valid")
            .with_endpoint(format!("{}/v1beta", server.uri()));

    // 150 inputs => one full 100-input batch plus a 50-input batch.
    let inputs: Vec<String> = (0..150).map(|index| format!("doc-{index}")).collect();
    let embeddings = embedder
        .embed(&inputs)
        .await
        .expect("chunked v2 embedding request should succeed");

    assert_eq!(embeddings.len(), 150);
    for (index, embedding) in embeddings.iter().enumerate() {
        assert_eq!(
            embedding[0], index as f32,
            "output at position {index} must correspond to input {index}"
        );
    }

    let requests = server
        .received_requests()
        .await
        .expect("mock server should expose received requests");
    assert_eq!(
        requests.len(),
        2,
        "150 inputs split into two batch requests"
    );
    let mut sizes: Vec<usize> = requests
        .iter()
        .map(|request| {
            let body: Value =
                serde_json::from_slice(&request.body).expect("batch request body should be JSON");
            body["requests"].as_array().expect("requests array").len()
        })
        .collect();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![50, 100]);
}

/// Responds to a `:batchEmbedContents` call with one vector per requested item,
/// marking position 0 with the numeric suffix parsed from each input's text so
/// output ordering can be asserted across chunk boundaries.
struct MarkedBatchEmbeddings {
    dim: usize,
}

impl Respond for MarkedBatchEmbeddings {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("batch request JSON");
        let embeddings: Vec<Vec<f32>> = body["requests"]
            .as_array()
            .expect("requests array")
            .iter()
            .map(|entry| {
                let text = entry["content"]["parts"][0]["text"]
                    .as_str()
                    .expect("entry text");
                let marker = text
                    .rsplit_once("doc-")
                    .and_then(|(_, suffix)| suffix.parse::<f32>().ok())
                    .expect("input text should carry a numeric marker");
                let mut vector = vec![0.0_f32; self.dim];
                vector[0] = marker;
                vector
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(batch_response(&embeddings))
    }
}

fn batch_response(embeddings: &[Vec<f32>]) -> Value {
    serde_json::json!({
        "embeddings": embeddings
            .iter()
            .map(|values| serde_json::json!({ "values": values }))
            .collect::<Vec<_>>(),
    })
}

async fn only_request(server: &MockServer) -> wiremock::Request {
    let requests = server.received_requests().await;
    let requests = requests.expect("mock server should expose received requests");
    assert_eq!(requests.len(), 1);
    requests
        .into_iter()
        .next()
        .expect("one request should exist")
}
