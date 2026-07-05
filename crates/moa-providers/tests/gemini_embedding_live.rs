//! Live Gemini embedding provider coverage.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_GEMINI_EMBEDDING_TESTS=1` because they call a billed external API.

use moa_core::traits::EmbeddingProvider;
use moa_providers::{EmbedderConstructionRole, GeminiEmbeddingEmbedder};

const LIVE_DIMENSIONS: usize = 1024;

fn live_gemini_embedding_requested() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_GEMINI_EMBEDDING_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn live_gemini_key() -> Option<String> {
    if !live_gemini_embedding_requested() {
        return None;
    }

    let api_key = std::env::var("MOA_GOOGLE_API_KEY")
        .expect("MOA_GOOGLE_API_KEY is required when MOA_RUN_LIVE_GEMINI_EMBEDDING_TESTS=1");
    Some(api_key)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_GEMINI_EMBEDDING_TESTS=1 and MOA_GOOGLE_API_KEY"]
async fn gemini_embedding_2_returns_1024_dimensional_float_embeddings() {
    let Some(api_key) = live_gemini_key() else {
        return;
    };
    let embedder = GeminiEmbeddingEmbedder::new(
        api_key,
        LIVE_DIMENSIONS,
        EmbedderConstructionRole::Retrieval,
    )
    .expect("Gemini embedding provider should build");
    let texts = vec![
        "MOA stores graph memory in PostgreSQL with row-level security.".to_string(),
        "Gemini Embedding 2 should return finite text embeddings.".to_string(),
    ];

    let embeddings = embedder
        .embed(&texts)
        .await
        .expect("Gemini embedding live request should succeed");

    assert_eq!(embedder.model_id(), "gemini-embedding-2");
    assert_eq!(embedder.dimensions(), LIVE_DIMENSIONS);
    assert_eq!(embeddings.len(), texts.len());
    for embedding in &embeddings {
        assert_eq!(embedding.len(), LIVE_DIMENSIONS);
        assert!(embedding.iter().all(|value| value.is_finite()));
        assert!(embedding.iter().any(|value| *value != 0.0));
    }
    assert_ne!(embeddings[0], embeddings[1]);
}
