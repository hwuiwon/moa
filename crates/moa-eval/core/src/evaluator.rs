//! Evaluator traits for scoring agent outputs after a suite run.

use async_trait::async_trait;

use crate::error::Result;
use crate::results::{EvalResult, EvalScore};
use crate::types::TestCase;

/// Scores a single evaluation result against a test case definition.
#[async_trait]
pub trait Evaluator: Send + Sync {
    /// Returns the human-readable evaluator name.
    fn name(&self) -> &str;

    /// Produces one or more scores for a completed evaluation result.
    async fn evaluate(&self, test_case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>>;
}
