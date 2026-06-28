//! Deterministic embedding provider used by tests.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use async_trait::async_trait;
use moa_core::Result;
use moa_core::traits::EmbeddingProvider;

/// Deterministic embedding provider used by tests.
#[derive(Clone, Debug)]
pub struct MockEmbedding {
    dimensions: usize,
    model: String,
}

impl MockEmbedding {
    /// Creates a deterministic mock embedding provider.
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(8),
            model: format!("mock-embedding-{dimensions}"),
        }
    }

    fn embed_one(&self, input: &str) -> Vec<f32> {
        let mut vector = vec![0.0; self.dimensions];
        let mut token_count = 0_u32;

        for token in tokenize(input) {
            token_count += 1;
            add_feature(&mut vector, &token, 1.0);
            for alias in token_aliases(&token) {
                add_feature(&mut vector, alias, 0.75);
            }
            for trigram in char_trigrams(&token) {
                add_feature(&mut vector, &trigram, 0.2);
            }
        }

        if token_count == 0 {
            vector[0] = 1.0;
            return vector;
        }

        normalize(&mut vector);
        vector
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbedding {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|input| self.embed_one(input)).collect())
    }
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_ascii_lowercase())
        .collect()
}

fn char_trigrams(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 3 {
        return vec![token.to_string()];
    }

    chars
        .windows(3)
        .map(|window| window.iter().collect::<String>())
        .collect()
}

fn token_aliases(token: &str) -> &'static [&'static str] {
    match token {
        "auth" | "authenticate" | "authentication" => &["oauth", "token", "identity"],
        "identity" => &["auth", "oauth", "token"],
        "oauth" | "oauth2" => &["auth", "token", "refresh"],
        "jwt" => &["token", "auth"],
        "refresh" => &["token", "oauth", "rotation"],
        "rotation" => &["refresh", "token"],
        "token" | "tokens" => &["oauth", "auth", "jwt"],
        "cache" | "caching" => &["reuse", "storage"],
        "replay" => &["history", "session", "events"],
        _ => &[],
    }
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let mut hasher = DefaultHasher::new();
    feature.hash(&mut hasher);
    let idx = (hasher.finish() as usize) % vector.len();
    vector[idx] += weight;
}

fn normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt() as f32;
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EmbeddingProvider, MockEmbedding};

    #[tokio::test]
    async fn mock_embedding_is_deterministic() {
        let provider = MockEmbedding::new(64);
        let left = provider
            .embed(&[String::from("oauth refresh token")])
            .await
            .expect("embed");
        let right = provider
            .embed(&[String::from("oauth refresh token")])
            .await
            .expect("embed");

        assert_eq!(left, right);
    }
}
