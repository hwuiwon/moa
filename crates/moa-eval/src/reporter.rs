//! Reporter traits for emitting evaluation summaries to different sinks.

use async_trait::async_trait;
use moa_eval_core::engine::EvalRun;
use moa_eval_core::{AgentConfig, Result, TestSuite};

/// Consumes the results of a completed suite execution.
#[async_trait]
pub trait Reporter: Send + Sync {
    /// Reports the collected suite run to an output sink.
    async fn report(&self, suite: &TestSuite, configs: &[AgentConfig], run: &EvalRun)
    -> Result<()>;
}
