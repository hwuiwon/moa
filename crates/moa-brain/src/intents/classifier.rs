//! Embedding-based nearest-centroid intent classification.

use std::sync::Arc;

use moa_core::{MoaError, Result, TenantIntent};
use moa_providers::EmbeddingProvider;
use moa_session::PostgresSessionStore;

/// Default cosine-distance threshold for active intent classification.
pub const DEFAULT_INTENT_DISTANCE_THRESHOLD: f64 = 0.35;

/// Classifies task segments against active tenant intents.
pub struct IntentClassifier {
    session_store: Arc<PostgresSessionStore>,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    threshold: f64,
}

impl IntentClassifier {
    /// Creates a classifier with the default cosine-distance threshold.
    #[must_use]
    pub fn new(
        session_store: Arc<PostgresSessionStore>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self::with_threshold(
            session_store,
            embedding_provider,
            DEFAULT_INTENT_DISTANCE_THRESHOLD,
        )
    }

    /// Creates a classifier with an explicit cosine-distance threshold.
    #[must_use]
    pub fn with_threshold(
        session_store: Arc<PostgresSessionStore>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        threshold: f64,
    ) -> Self {
        Self {
            session_store,
            embedding_provider,
            threshold,
        }
    }

    /// Classifies a segment against the tenant's active intents.
    pub async fn classify(
        &self,
        tenant_id: &str,
        task_summary: &str,
        first_user_message: &str,
    ) -> Result<Option<(TenantIntent, f64)>> {
        let text = classification_text(task_summary, first_user_message);
        if text.is_empty() {
            return Ok(None);
        }

        let embeddings = self.embedding_provider.embed(&[text]).await?;
        let embedding = embeddings.into_iter().next().ok_or_else(|| {
            MoaError::ProviderError(
                "embedding provider returned zero intent embeddings".to_string(),
            )
        })?;
        let matches = self
            .session_store
            .get_intent_by_embedding(tenant_id, &embedding, 3)
            .await?;

        Ok(best_within_threshold(&matches, self.threshold))
    }
}

fn classification_text(task_summary: &str, first_user_message: &str) -> String {
    [task_summary.trim(), first_user_message.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn best_within_threshold(
    matches: &[(TenantIntent, f64)],
    threshold: f64,
) -> Option<(TenantIntent, f64)> {
    matches.first().and_then(|(intent, distance)| {
        if *distance < threshold {
            Some((intent.clone(), 1.0 - distance))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use moa_core::{IntentSource, IntentStatus};
    use uuid::Uuid;

    use super::{best_within_threshold, classification_text};

    fn intent(label: &str) -> moa_core::TenantIntent {
        moa_core::TenantIntent {
            id: Uuid::now_v7(),
            tenant_id: "tenant".to_string(),
            label: label.to_string(),
            description: None,
            status: IntentStatus::Active,
            source: IntentSource::Manual,
            catalog_ref: None,
            example_queries: Vec::new(),
            embedding: None,
            segment_count: 0,
            resolution_rate: None,
        }
    }

    #[test]
    fn exact_match_returns_high_confidence() {
        let selected = best_within_threshold(&[(intent("debugging"), 0.02)], 0.35)
            .expect("intent should match threshold");
        assert_eq!(selected.0.label, "debugging");
        assert!((selected.1 - 0.98).abs() < f64::EPSILON);
    }

    #[test]
    fn embedding_below_threshold_returns_none() {
        let selected = best_within_threshold(&[(intent("debugging"), 0.35)], 0.35);
        assert!(selected.is_none());
    }

    #[test]
    fn empty_inputs_do_not_emit_spurious_spaces() {
        assert_eq!(
            classification_text("  Fix tests ", " "),
            "Fix tests".to_string()
        );
    }
}
