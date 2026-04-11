//! Reporter traits for emitting evaluation summaries to different sinks.

use async_trait::async_trait;

use crate::error::Result;
use crate::results::EvalResult;
use crate::types::{AgentConfig, TestSuite};

/// Consumes the results of a completed suite execution.
#[async_trait]
pub trait Reporter: Send + Sync {
    /// Reports the collected suite results to an output sink.
    async fn report(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        results: &[EvalResult],
    ) -> Result<()>;
}
