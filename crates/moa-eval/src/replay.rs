//! Shared replay scoring primitives.

use std::collections::BTreeSet;

use uuid::Uuid;

/// Configuration for replaying one stored dataset.
#[derive(Clone, Debug)]
pub struct ReplayConfig {
    /// Dataset to replay.
    pub dataset_id: Uuid,
    /// Run identifier for grouping emitted scores.
    pub run_id: Uuid,
    /// Optional model override label.
    pub model_override: Option<String>,
    /// Optional embedder override label.
    pub embedder_override: Option<String>,
    /// Optional item cap.
    pub limit: Option<usize>,
}

/// Computes a token-overlap F1 score.
#[must_use]
pub fn token_f1(actual: &str, expected: &str) -> f64 {
    let actual_tokens = normalized_tokens(actual);
    let expected_tokens = normalized_tokens(expected);
    if actual_tokens.is_empty() || expected_tokens.is_empty() {
        return 0.0;
    }

    let overlap = actual_tokens.intersection(&expected_tokens).count() as f64;
    if overlap == 0.0 {
        return 0.0;
    }
    let precision = overlap / actual_tokens.len() as f64;
    let recall = overlap / expected_tokens.len() as f64;
    2.0 * precision * recall / (precision + recall)
}

fn normalized_tokens(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::token_f1;

    #[test]
    fn token_f1_scores_overlap() {
        assert_eq!(token_f1("alpha beta", "gamma delta"), 0.0);
        assert!(token_f1("alpha beta", "alpha gamma") > 0.0);
        assert_eq!(token_f1("alpha beta", "alpha beta"), 1.0);
    }
}
