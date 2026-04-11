//! Langfuse score reporter.

use serde_json::json;

use super::required_env_var;
use crate::engine::EvalRun;
use crate::{AgentConfig, Reporter, Result, ScoreValue, TestSuite};

/// Posts evaluator scores to Langfuse so they appear alongside eval traces.
pub struct LangfuseReporter {
    /// Langfuse API base URL.
    pub base_url: String,
    /// Langfuse public key.
    pub public_key: String,
    /// Langfuse secret key.
    pub secret_key: String,
}

impl LangfuseReporter {
    /// Builds a reporter from `LANGFUSE_BASE_URL`, `LANGFUSE_PUBLIC_KEY`, and `LANGFUSE_SECRET_KEY`.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            base_url: required_env_var("LANGFUSE_BASE_URL")?,
            public_key: required_env_var("LANGFUSE_PUBLIC_KEY")?,
            secret_key: required_env_var("LANGFUSE_SECRET_KEY")?,
        })
    }
}

#[async_trait::async_trait]
impl Reporter for LangfuseReporter {
    async fn report(
        &self,
        _suite: &TestSuite,
        _configs: &[AgentConfig],
        run: &EvalRun,
    ) -> Result<()> {
        let client = reqwest::Client::new();
        let endpoint = format!("{}/api/public/scores", self.base_url.trim_end_matches('/'));

        for result in &run.results {
            let Some(trace_id) = &result.trace_id else {
                continue;
            };

            for score in &result.scores {
                let (value, data_type) = match &score.value {
                    ScoreValue::Numeric(value) => (json!(value), "NUMERIC"),
                    ScoreValue::Boolean(value) => (json!(value), "BOOLEAN"),
                    ScoreValue::Categorical(value) => (json!(value), "CATEGORICAL"),
                };
                let payload = json!({
                    "traceId": trace_id,
                    "name": score.name,
                    "value": value,
                    "dataType": data_type,
                    "source": "API",
                    "comment": score.comment,
                });

                client
                    .post(&endpoint)
                    .basic_auth(&self.public_key, Some(&self.secret_key))
                    .json(&payload)
                    .send()
                    .await?
                    .error_for_status()?;
            }
        }

        Ok(())
    }
}
