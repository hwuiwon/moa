//! Mock PII classifier for deterministic ingestion tests.

use crate::{Error, PiiClassifier, PiiResult};

/// Deterministic classifier that always returns the configured result.
#[derive(Debug, Clone)]
pub struct MockClassifier {
    /// Result returned for every input.
    pub fixed: PiiResult,
}

#[async_trait::async_trait]
impl PiiClassifier for MockClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, Error> {
        Ok(self.fixed.clone())
    }
}
