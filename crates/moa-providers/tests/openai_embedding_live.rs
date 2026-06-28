//! Live OpenAI embedding provider coverage.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_OPENAI_EMBEDDING_TESTS=1` because they call a billed external API.

use moa_core::traits::EmbeddingProvider;
use moa_providers::OpenAIEmbedding;

const LIVE_DIMENSIONS: usize = 1536;

fn live_openai_embedding_requested() -> bool {
    matches!(
        std::env::var("MOA_RUN_LIVE_OPENAI_EMBEDDING_TESTS").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn live_openai_key() -> Option<String> {
    if !live_openai_embedding_requested() {
        return None;
    }

    let api_key = std::env::var("MOA_OPENAI_API_KEY")
        .expect("MOA_OPENAI_API_KEY is required when MOA_RUN_LIVE_OPENAI_EMBEDDING_TESTS=1");
    Some(api_key)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_OPENAI_EMBEDDING_TESTS=1 and MOA_OPENAI_API_KEY"]
async fn openai_text_embedding_3_small_returns_1536_dimensional_float_embeddings() {
    let Some(api_key) = live_openai_key() else {
        return;
    };
    let embedder = OpenAIEmbedding::new(api_key, "text-embedding-3-small")
        .expect("OpenAI embedding provider should build");
    let texts = vec![
        "MOA stores graph memory in PostgreSQL with row-level security.".to_string(),
        "OpenAI text-embedding-3-small should return finite text embeddings.".to_string(),
    ];

    let embeddings = embedder
        .embed(&texts)
        .await
        .expect("OpenAI embedding live request should succeed");

    assert_eq!(embedder.model_id(), "text-embedding-3-small");
    assert_eq!(embedder.dimensions(), LIVE_DIMENSIONS);
    assert_eq!(embeddings.len(), texts.len());
    for embedding in &embeddings {
        assert_eq!(embedding.len(), LIVE_DIMENSIONS);
        assert!(embedding.iter().all(|value| value.is_finite()));
        assert!(embedding.iter().any(|value| *value != 0.0));
    }
    assert_ne!(embeddings[0], embeddings[1]);
}
