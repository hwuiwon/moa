//! Wiremock offline counterpart for ZeroEntropy embedding provider coverage.

use moa_core::{MoaError, traits::EmbeddingProvider};
use moa_providers::ZeroEntropyEmbedding;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn zeroentropy_embedding_offline_batches_inputs_and_returns_configured_dimensions() {
    // Pins: ZeroEntropy calls the v1 embed endpoint with document input and float output dimensions.
    let server = MockServer::start().await;
    let first_chunk = texts(0..100);
    let second_chunk = texts(100..101);
    mount_embed_response(&server, &first_chunk, embeddings(first_chunk.len(), 40)).await;
    mount_embed_response(&server, &second_chunk, embeddings(second_chunk.len(), 40)).await;
    let provider = provider(&server, 40);
    let mut inputs = first_chunk;
    inputs.extend(second_chunk);

    let embeddings = provider
        .embed(&inputs)
        .await
        .expect("wiremock ZeroEntropy embed request should succeed");

    assert_eq!(provider.model_id(), "zembed-1");
    assert_eq!(provider.dimensions(), 40);
    assert_eq!(embeddings.len(), inputs.len());
    assert!(embeddings.iter().all(|embedding| embedding.len() == 40));
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose ZeroEntropy embed requests");
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: Value =
            serde_json::from_slice(&request.body).expect("ZeroEntropy request body should be JSON");
        assert_eq!(body["model"], json!("zembed-1"));
        assert_eq!(body["input_type"], json!("document"));
        assert_eq!(body["encoding_format"], json!("float"));
        assert_eq!(body["dimensions"], json!(40));
    }
}

#[tokio::test]
async fn zeroentropy_embedding_offline_empty_batch_skips_http_call() {
    // Pins: empty embedding batches return locally so providers that reject [] are normalized.
    let server = MockServer::start().await;
    let provider = provider(&server, 40);

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
async fn zeroentropy_embedding_offline_http_error_surfaces_status() {
    // Pins: upstream ZeroEntropy failures preserve HTTP status for retry/failure classification.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let provider = provider(&server, 40);

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
async fn zeroentropy_embedding_offline_rejects_wrong_response_dimension() {
    // Pins: adapter rejects vectors that do not match the configured vector-space width.
    let server = MockServer::start().await;
    let inputs = texts(0..1);
    mount_embed_response(&server, &inputs, vec![vec![0.25; 39]]).await;
    let provider = provider(&server, 40);

    let error = provider
        .embed(&inputs)
        .await
        .expect_err("wrong vector dimension should fail");

    assert!(
        matches!(&error, MoaError::ProviderError(message) if message.contains("dimension mismatch")),
        "expected dimension mismatch provider error, got {error:?}"
    );
}

#[test]
fn zeroentropy_embedding_offline_rejects_unsupported_dimensions() {
    // Pins: zembed-1 dimensions stay within the provider-supported vector widths.
    let dimensions_result = ZeroEntropyEmbedding::new("test-key", "zembed-1")
        .expect("provider config")
        .with_dimensions(1_536);

    assert!(
        matches!(&dimensions_result, Err(MoaError::ConfigError(message)) if message.contains("one of")),
        "expected dimension config error, got success or another error"
    );
}

async fn mount_embed_response(server: &MockServer, input: &[String], embeddings: Vec<Vec<f32>>) {
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "zembed-1",
            "input": input,
            "input_type": "document",
            "dimensions": 40,
            "encoding_format": "float"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": embeddings
                .into_iter()
                .map(|embedding| json!({ "embedding": embedding }))
                .collect::<Vec<Value>>(),
            "usage": {
                "total_bytes": 1,
                "total_tokens": 1
            }
        })))
        .mount(server)
        .await;
}

fn provider(server: &MockServer, dimensions: usize) -> ZeroEntropyEmbedding {
    ZeroEntropyEmbedding::new("test-key", "zembed-1")
        .expect("provider config")
        .with_embeddings_url(format!("{}/v1/models/embed", server.uri()))
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
