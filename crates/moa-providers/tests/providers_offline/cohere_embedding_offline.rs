//! Wiremock offline counterpart for Cohere Embed v4 provider coverage.

use moa_core::{MoaError, traits::EmbeddingProvider};
use moa_providers::CohereEmbedding;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cohere_embedding_offline_batches_inputs_and_returns_configured_dimensions() {
    // Pins: Cohere Embed v4 provider calls use v2 text batching and float output dimensions.
    let server = MockServer::start().await;
    let first_chunk = texts(0..96);
    let second_chunk = texts(96..97);
    mount_embed_response(&server, &first_chunk, embeddings(first_chunk.len(), 4)).await;
    mount_embed_response(&server, &second_chunk, embeddings(second_chunk.len(), 4)).await;
    let provider = provider(&server, 4);
    let mut inputs = first_chunk;
    inputs.extend(second_chunk);

    let embeddings = provider
        .embed(&inputs)
        .await
        .expect("wiremock Cohere embed request should succeed");

    assert_eq!(provider.model_id(), "embed-v4.0");
    assert_eq!(provider.dimensions(), 4);
    assert_eq!(embeddings.len(), inputs.len());
    assert!(embeddings.iter().all(|embedding| embedding.len() == 4));
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose Cohere embed requests");
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: Value =
            serde_json::from_slice(&request.body).expect("Cohere request body should be JSON");
        assert_eq!(body["model"], json!("embed-v4.0"));
        assert_eq!(body["input_type"], json!("search_document"));
        assert_eq!(body["embedding_types"], json!(["float"]));
        assert_eq!(body["output_dimension"], json!(4));
    }
}

#[tokio::test]
async fn cohere_embedding_offline_empty_batch_skips_http_call() {
    // Pins: empty embedding batches return locally so providers that reject [] are normalized.
    let server = MockServer::start().await;
    let provider = provider(&server, 4);

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
async fn cohere_embedding_offline_http_error_surfaces_status() {
    // Pins: upstream Cohere failures preserve HTTP status for retry/failure classification.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let provider = provider(&server, 4);

    let error = provider
        .embed(&["hello".to_string()])
        .await
        .expect_err("401 should fail");

    assert!(
        matches!(error, MoaError::HttpStatus { status: 401, .. }),
        "expected HTTP 401, got {error:?}"
    );
}

#[tokio::test]
async fn cohere_embedding_offline_rejects_wrong_response_dimension() {
    // Pins: adapter rejects vectors that do not match the configured vector-space width.
    let server = MockServer::start().await;
    let inputs = texts(0..1);
    mount_embed_response(&server, &inputs, vec![vec![0.25; 3]]).await;
    let provider = provider(&server, 4);

    let error = provider
        .embed(&inputs)
        .await
        .expect_err("wrong vector dimension should fail");

    assert!(
        matches!(&error, MoaError::ProviderError(message) if message.contains("dimension mismatch")),
        "expected dimension mismatch provider error, got {error:?}"
    );
}

#[tokio::test]
async fn cohere_embedding_offline_preserves_input_order_across_concurrent_chunks() {
    // Pins: chunks that run concurrently and may complete out of order still
    // reassemble into input order (each output vector is marked with its input
    // index so any reordering would be observable).
    let server = MockServer::start().await;
    // 202 inputs => three chunks of 96, 96, and 10 that overflow the concurrency
    // window and can therefore complete out of order.
    let inputs: Vec<String> = (0..202).map(|index| format!("doc-{index}")).collect();
    for range in [0..96, 96..192, 192..202] {
        let chunk = inputs[range.clone()].to_vec();
        mount_embed_response(&server, &chunk, marked_embeddings(range)).await;
    }
    let provider = provider(&server, 4);

    let embeddings = provider
        .embed(&inputs)
        .await
        .expect("concurrent chunked Cohere embed request should succeed");

    assert_eq!(embeddings.len(), inputs.len());
    for (index, embedding) in embeddings.iter().enumerate() {
        assert_eq!(
            embedding[0], index as f32,
            "output at position {index} must correspond to input {index}"
        );
    }
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose Cohere embed requests");
    assert_eq!(requests.len(), 3, "three chunks issue three requests");
}

async fn mount_embed_response(server: &MockServer, texts: &[String], embeddings: Vec<Vec<f32>>) {
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "embed-v4.0",
            "texts": texts,
            "input_type": "search_document",
            "embedding_types": ["float"],
            "output_dimension": 4
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": {
                "float": embeddings
            }
        })))
        .mount(server)
        .await;
}

fn provider(server: &MockServer, dimensions: usize) -> CohereEmbedding {
    CohereEmbedding::new("test-key", "embed-v4.0")
        .expect("provider config")
        .with_embeddings_url(format!("{}/v2/embed", server.uri()))
        .with_dimensions(dimensions)
        .expect("test dimensions should be valid")
}

fn texts(range: std::ops::Range<usize>) -> Vec<String> {
    range
        .map(|index| format!("retrieval document {index}"))
        .collect()
}

fn embeddings(count: usize, dimensions: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|index| {
            let mut embedding = vec![0.0; dimensions];
            embedding[index % dimensions] = 1.0;
            embedding
        })
        .collect()
}

/// Builds width-4 vectors whose position 0 carries the global input index so
/// cross-chunk ordering can be asserted after concurrent completion.
fn marked_embeddings(range: std::ops::Range<usize>) -> Vec<Vec<f32>> {
    range
        .map(|index| {
            let mut embedding = vec![0.0_f32; 4];
            embedding[0] = index as f32;
            embedding
        })
        .collect()
}
