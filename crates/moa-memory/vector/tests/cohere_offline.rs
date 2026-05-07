//! Wiremock offline counterpart for Cohere Embed v4 live coverage.

use moa_core::traits::EmbeddingProvider;
use moa_memory_vector::{CohereV4Embedder, VECTOR_DIMENSION};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cohere_embed_offline_returns_1024_dimensional_float_embeddings() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "embed-v4.0",
            "input_type": "search_document",
            "output_dimension": VECTOR_DIMENSION
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": {
                "float": [
                    vec![0.125_f32; VECTOR_DIMENSION],
                    basis_vector(3)
                ]
            }
        })))
        .mount(&server)
        .await;

    let embedder = CohereV4Embedder::new(SecretString::from("test-key"))
        .with_endpoint(format!("{}/v2/embed", server.uri()));
    let texts = vec![
        "MOA stores graph memory in PostgreSQL with row-level security.".to_string(),
        "The deployment target for this validation sentence is fly.io.".to_string(),
    ];

    let embeddings = embedder
        .embed(&texts)
        .await
        .expect("wiremock Cohere embed request should succeed");

    assert_eq!(embedder.model_id(), "cohere-embed-v4");
    assert_eq!(embedder.dimensions(), VECTOR_DIMENSION);
    assert_eq!(embeddings.len(), texts.len());
    for embedding in &embeddings {
        assert_eq!(embedding.len(), VECTOR_DIMENSION);
        assert!(embedding.iter().all(|value| value.is_finite()));
        assert!(embedding.iter().any(|value| *value != 0.0));
    }
    assert_ne!(embeddings[0], embeddings[1]);
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; VECTOR_DIMENSION];
    embedding[index % VECTOR_DIMENSION] = 1.0;
    embedding
}
