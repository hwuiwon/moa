//! Wiremock offline counterpart for the OpenAI `/v1/embeddings` provider.
//!
//! Mirrors `cohere_embedding_offline.rs` but pins the OpenAI-unique behaviour:
//! the provider re-sorts the response `data` array by its `index` field back to
//! input order, and rejects vectors whose width does not match the model's fixed
//! dimensionality.

use moa_core::{MoaError, traits::EmbeddingProvider};
use moa_providers::OpenAIEmbedding;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "text-embedding-3-small";

#[tokio::test]
async fn openai_embedding_offline_reorders_out_of_order_indices_to_input_order() {
    // Pins: OpenAI returns embeddings tagged with `index`; the provider must
    // re-sort them so the Nth output corresponds to the Nth input even when the
    // API streams them out of order, and it batches all inputs in one request.
    let server = MockServer::start().await;
    let provider = provider(&server);
    let dimensions = provider.dimensions();
    // The response is intentionally shuffled: index 1 before index 0. Each
    // embedding is uniquely marked at position 0 with its input index.
    let shuffled = json!({
        "data": [
            { "index": 1, "embedding": marked_vector(1, dimensions) },
            { "index": 0, "embedding": marked_vector(0, dimensions) },
        ]
    });
    mount_embed_response(
        &server,
        &["first".to_string(), "second".to_string()],
        shuffled,
    )
    .await;

    let embeddings = provider
        .embed(&["first".to_string(), "second".to_string()])
        .await
        .expect("wiremock OpenAI embed request should succeed");

    assert_eq!(embeddings.len(), 2);
    // Despite the shuffled response, output[0] is the index-0 vector.
    assert_eq!(
        embeddings[0][0], 0.0,
        "first output should be the index-0 vector"
    );
    assert_eq!(
        embeddings[1][0], 1.0,
        "second output should be the index-1 vector"
    );
    assert!(
        embeddings
            .iter()
            .all(|embedding| embedding.len() == dimensions)
    );

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose OpenAI embed requests");
    assert_eq!(
        requests.len(),
        1,
        "all inputs go in one OpenAI embeddings request"
    );
    let body: Value =
        serde_json::from_slice(&requests[0].body).expect("OpenAI request body should be JSON");
    assert_eq!(body["model"], json!(MODEL));
    assert_eq!(body["input"], json!(["first", "second"]));
    assert_eq!(body["encoding_format"], json!("float"));
}

#[tokio::test]
async fn openai_embedding_offline_rejects_wrong_response_dimension() {
    // Pins: a vector whose width != the model dimensionality is a typed error,
    // not silently accepted (matches the Cohere/Gemini/ZeroEntropy embedders).
    let server = MockServer::start().await;
    let provider = provider(&server);
    let wrong_width = json!({
        "data": [
            { "index": 0, "embedding": vec![0.25_f32; 3] },
        ]
    });
    mount_embed_response(&server, &["hello".to_string()], wrong_width).await;

    let error = provider
        .embed(&["hello".to_string()])
        .await
        .expect_err("wrong vector width should fail");

    assert!(
        matches!(&error, MoaError::ProviderError(message) if message.contains("dimension mismatch")),
        "expected dimension mismatch provider error, got {error:?}"
    );
}

#[tokio::test]
async fn openai_embedding_offline_empty_batch_skips_http_call() {
    // Pins: empty embedding batches return locally without an HTTP round-trip.
    let server = MockServer::start().await;
    let provider = provider(&server);

    let embeddings = provider
        .embed(&[])
        .await
        .expect("empty batch should succeed without HTTP");

    assert!(embeddings.is_empty());
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose request log");
    assert!(requests.is_empty());
}

#[tokio::test]
async fn openai_embedding_offline_http_error_surfaces_status() {
    // Pins: upstream OpenAI failures preserve HTTP status for failure classification.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let provider = provider(&server);

    let error = provider
        .embed(&["hello".to_string()])
        .await
        .expect_err("401 should fail");

    assert!(
        matches!(error, MoaError::HttpStatus { status: 401, .. }),
        "expected HTTP 401, got {error:?}"
    );
}

async fn mount_embed_response(server: &MockServer, inputs: &[String], body: Value) {
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": MODEL,
            "input": inputs,
            "encoding_format": "float",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn provider(server: &MockServer) -> OpenAIEmbedding {
    OpenAIEmbedding::new("test-key", MODEL)
        .expect("provider config")
        .with_embeddings_url(format!("{}/v1/embeddings", server.uri()))
}

/// Builds a `dimensions`-wide vector marked at position 0 with `marker` so each
/// input's embedding is distinguishable after re-sorting.
fn marked_vector(marker: usize, dimensions: usize) -> Vec<f32> {
    let mut embedding = vec![0.0_f32; dimensions];
    embedding[0] = marker as f32;
    embedding
}
