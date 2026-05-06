//! Live Cohere Embed v4 coverage.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_COHERE_TESTS=1` because they call a billed external API.

use moa_memory_vector::{CohereV4Embedder, Embedder, VECTOR_DIMENSION};
use secrecy::SecretString;

fn live_cohere_requested() -> bool {
    matches!(
        std::env::var("MOA_RUN_LIVE_COHERE_TESTS").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn live_cohere_key() -> Option<SecretString> {
    if !live_cohere_requested() {
        return None;
    }

    let api_key = std::env::var("COHERE_API_KEY")
        .or_else(|_| std::env::var("MOA_COHERE_API_KEY"))
        .expect(
            "COHERE_API_KEY or MOA_COHERE_API_KEY is required when \
             MOA_RUN_LIVE_COHERE_TESTS=1",
        );
    Some(SecretString::from(api_key))
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_COHERE_TESTS=1 and COHERE_API_KEY or MOA_COHERE_API_KEY"]
async fn cohere_embed_v4_returns_1024_dimensional_float_embeddings() {
    let Some(api_key) = live_cohere_key() else {
        return;
    };
    let embedder = CohereV4Embedder::new(api_key);
    let texts = vec![
        "MOA stores graph memory in PostgreSQL with row-level security.".to_string(),
        "The deployment target for this validation sentence is fly.io.".to_string(),
    ];

    let embeddings = embedder
        .embed(&texts)
        .await
        .expect("Cohere Embed v4 live request should succeed");

    assert_eq!(embedder.model_name(), "cohere-embed-v4");
    assert_eq!(embedder.dimension(), VECTOR_DIMENSION);
    assert_eq!(embeddings.len(), texts.len());
    for embedding in &embeddings {
        assert_eq!(embedding.len(), VECTOR_DIMENSION);
        assert!(embedding.iter().all(|value| value.is_finite()));
        assert!(embedding.iter().any(|value| *value != 0.0));
    }
    assert_ne!(embeddings[0], embeddings[1]);
}
