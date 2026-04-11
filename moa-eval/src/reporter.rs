//! Reporter traits for emitting evaluation summaries to different sinks.

use async_trait::async_trait;

use crate::engine::EvalRun;
use crate::error::Result;
use crate::types::{AgentConfig, TestSuite};

/// Consumes the results of a completed suite execution.
#[async_trait]
pub trait Reporter: Send + Sync {
    /// Reports the collected suite run to an output sink.
    async fn report(&self, suite: &TestSuite, configs: &[AgentConfig], run: &EvalRun)
    -> Result<()>;
}
