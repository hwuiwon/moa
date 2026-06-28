// Live counterpart: see zeroentropy_embedding_offline.rs for the wiremock version that runs in PR CI.

//! Live ZeroEntropy zembed-1 provider coverage.
//!
//! These tests are intentionally ignored and additionally gated by
//! `MOA_RUN_LIVE_ZEROENTROPY_TESTS=1` because they call a billed external API.

use moa_core::traits::EmbeddingProvider;
use moa_providers::ZeroEntropyEmbedding;

const LIVE_DIMENSIONS: usize = 1_280;

fn live_zeroentropy_requested() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_ZEROENTROPY_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn live_zeroentropy_key() -> Option<String> {
    if !live_zeroentropy_requested() {
        return None;
    }

    let api_key = std::env::var("MOA_ZEROENTROPY_API_KEY")
        .expect("MOA_ZEROENTROPY_API_KEY is required when MOA_RUN_LIVE_ZEROENTROPY_TESTS=1");
    Some(api_key)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_ZEROENTROPY_TESTS=1 and MOA_ZEROENTROPY_API_KEY"]
async fn zeroentropy_zembed_1_provider_returns_configured_float_embeddings() {
    let Some(api_key) = live_zeroentropy_key() else {
        return;
    };
    let embedder =
        ZeroEntropyEmbedding::new(api_key, "zembed-1").expect("ZeroEntropy provider should build");
    let texts = vec![
        "MOA stores graph memory in PostgreSQL with row-level security.".to_string(),
        "ZeroEntropy zembed-1 should return finite text embeddings.".to_string(),
    ];

    let embeddings = embedder
        .embed(&texts)
        .await
        .expect("ZeroEntropy zembed-1 live request should succeed");

    assert_eq!(embedder.model_id(), "zembed-1");
    assert_eq!(embedder.dimensions(), LIVE_DIMENSIONS);
    assert_eq!(embeddings.len(), texts.len());
    for embedding in &embeddings {
        assert_eq!(embedding.len(), LIVE_DIMENSIONS);
        assert!(embedding.iter().all(|value| value.is_finite()));
        assert!(embedding.iter().any(|value| *value != 0.0));
    }
    assert_ne!(embeddings[0], embeddings[1]);
}
