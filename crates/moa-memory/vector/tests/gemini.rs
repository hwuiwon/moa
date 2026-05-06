//! Deterministic Gemini embedder request-shape coverage.

use moa_memory_vector::{EmbedRole, Embedder, GeminiEmbeddingEmbedder};
use secrecy::SecretString;
use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn gemini_v2_uses_prompt_prefix_and_snake_case_output_dimensionality() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-embedding-2:embedContent"))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(128, 0.5)))
        .mount(&server)
        .await;

    let embedder =
        GeminiEmbeddingEmbedder::new(SecretString::from("test-key"), 128, EmbedRole::SearchQuery)
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
    assert_eq!(body["output_dimensionality"], 128);
    assert!(body.get("taskType").is_none());
    assert!(body.get("model").is_none());
    assert_eq!(
        body["content"]["parts"][0]["text"],
        "task: search result | query: oauth"
    );
}

#[tokio::test]
async fn gemini_v2_does_not_renormalize_server_output() {
    let server = MockServer::start().await;
    let mut values = vec![0.0_f32; 128];
    values[0] = 0.6;
    values[1] = 0.8;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-embedding-2:embedContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "embedding": { "values": values }
        })))
        .mount(&server)
        .await;

    let embedder = GeminiEmbeddingEmbedder::new(
        SecretString::from("test-key"),
        128,
        EmbedRole::Document { title: None },
    )
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
        body["content"]["parts"][0]["text"],
        "title: none | text: oauth"
    );
}

fn embedding_response(dim: usize, value: f32) -> Value {
    serde_json::json!({
        "embedding": {
            "values": vec![value; dim]
        }
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
