//! Placeholder built-in tools that reserve names for future implementations.

use async_trait::async_trait;
use moa_core::{MoaError, PolicyAction, Result, RiskLevel, ToolOutput};

use crate::router::{BuiltInTool, ToolContext, ToolDiffStrategy, ToolInputShape, ToolPolicySpec};

/// Stub built-in tool that reports the feature is not implemented yet.
pub struct StubTool {
    name: &'static str,
    description: &'static str,
    risk_level: RiskLevel,
}

impl StubTool {
    /// Creates a new stub tool definition.
    pub fn new(name: &'static str, description: &'static str, risk_level: RiskLevel) -> Self {
        Self {
            name,
            description,
            risk_level,
        }
    }
}

#[async_trait]
impl BuiltInTool for StubTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "url": { "type": "string" }
            },
            "additionalProperties": true
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        ToolPolicySpec {
            risk_level: self.risk_level.clone(),
            default_action: PolicyAction::RequireApproval,
            input_shape: match self.name {
                "web_fetch" => ToolInputShape::Url,
                "web_search" => ToolInputShape::Query,
                _ => ToolInputShape::Json,
            },
            diff_strategy: ToolDiffStrategy::None,
        }
    }

    async fn execute(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        Err(MoaError::Unsupported(format!(
            "{} is not implemented yet",
            self.name
        )))
    }
}
