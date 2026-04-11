//! JSON file reporter for completed eval runs.

use std::path::PathBuf;

use serde::Serialize;

use crate::engine::EvalRun;
use crate::{
    AgentConfig, EvalError, InstructionOverride, MemoryOverride, PermissionOverride, Reporter,
    Result, SkillOverride, TestCase, TestSuite, ToolOverride,
};

/// Writes the full suite run, suite metadata, and agent configs to a JSON file.
pub struct JsonReporter {
    /// Output file path.
    pub output_path: PathBuf,
    /// Whether to pretty-print the JSON output.
    pub pretty: bool,
}

#[derive(Debug, Serialize)]
struct JsonReportDocument<'a> {
    suite: JsonSuiteDocument<'a>,
    configs: Vec<JsonAgentConfigDocument<'a>>,
    run: &'a EvalRun,
}

#[derive(Debug, Serialize)]
struct JsonSuiteDocument<'a> {
    name: &'a str,
    description: &'a Option<String>,
    cases: &'a [TestCase],
    default_timeout_seconds: u64,
    tags: &'a [String],
}

#[derive(Debug, Serialize)]
struct JsonAgentConfigDocument<'a> {
    name: &'a str,
    model: &'a Option<String>,
    skills: &'a SkillOverride,
    memory: &'a MemoryOverride,
    instructions: &'a InstructionOverride,
    tools: &'a ToolOverride,
    permissions: &'a PermissionOverride,
    metadata: &'a std::collections::HashMap<String, String>,
}

#[async_trait::async_trait]
impl Reporter for JsonReporter {
    async fn report(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        run: &EvalRun,
    ) -> Result<()> {
        let document = JsonReportDocument {
            suite: JsonSuiteDocument {
                name: &suite.name,
                description: &suite.description,
                cases: &suite.cases,
                default_timeout_seconds: suite.default_timeout_seconds,
                tags: &suite.tags,
            },
            configs: configs
                .iter()
                .map(|config| JsonAgentConfigDocument {
                    name: &config.name,
                    model: &config.model,
                    skills: &config.skills,
                    memory: &config.memory,
                    instructions: &config.instructions,
                    tools: &config.tools,
                    permissions: &config.permissions,
                    metadata: &config.metadata,
                })
                .collect(),
            run,
        };
        let payload = if self.pretty {
            serde_json::to_vec_pretty(&document)?
        } else {
            serde_json::to_vec(&document)?
        };

        if let Some(parent) = self.output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| EvalError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }

        tokio::fs::write(&self.output_path, payload)
            .await
            .map_err(|source| EvalError::Io {
                path: self.output_path.clone(),
                source,
            })?;
        Ok(())
    }
}
