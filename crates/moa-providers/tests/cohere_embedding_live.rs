// Live counterpart: see cohere_embedding_offline.rs for the wiremock version that runs in PR CI.

//! Live Cohere Embed v4 provider coverage.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_COHERE_TESTS=1` because they call a billed external API.

use moa_core::traits::EmbeddingProvider;
use moa_providers::CohereEmbedding;

const LIVE_DIMENSIONS: usize = 1536;

fn live_cohere_requested() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_COHERE_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn live_cohere_key() -> Option<String> {
    if !live_cohere_requested() {
        return None;
    }

    let api_key = std::env::var("MOA_COHERE_API_KEY")
        .expect("MOA_COHERE_API_KEY is required when MOA_RUN_LIVE_COHERE_TESTS=1");
    Some(api_key)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_COHERE_TESTS=1 and MOA_COHERE_API_KEY"]
async fn cohere_embed_v4_provider_returns_1536_dimensional_float_embeddings() {
    let Some(api_key) = live_cohere_key() else {
        return;
    };
    let embedder = CohereEmbedding::new(api_key, "embed-v4.0")
        .expect("Cohere provider should build")
        .with_dimensions(LIVE_DIMENSIONS)
        .expect("live dimensions should be valid");
    let texts = vec![
        "MOA stores graph memory in PostgreSQL with row-level security.".to_string(),
        "Cohere Embed v4 should return finite text embeddings.".to_string(),
    ];

    let embeddings = embedder
        .embed(&texts)
        .await
        .expect("Cohere Embed v4 live request should succeed");

    assert_eq!(embedder.model_id(), "embed-v4.0");
    assert_eq!(embedder.dimensions(), LIVE_DIMENSIONS);
    assert_eq!(embeddings.len(), texts.len());
    for embedding in &embeddings {
        assert_eq!(embedding.len(), LIVE_DIMENSIONS);
        assert!(embedding.iter().all(|value| value.is_finite()));
        assert!(embedding.iter().any(|value| *value != 0.0));
    }
    assert_ne!(embeddings[0], embeddings[1]);
}
