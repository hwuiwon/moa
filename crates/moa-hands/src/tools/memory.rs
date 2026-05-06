//! Graph-backed built-in memory tool schemas.

use async_trait::async_trait;
use moa_core::{
    BuiltInTool, IdempotencyClass, PolicyAction, Result, RiskLevel, ToolContext, ToolDiffStrategy,
    ToolInputShape, ToolOutput, ToolPolicySpec,
};

fn fast_memory_policy() -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Medium,
        default_action: PolicyAction::Allow,
        input_shape: ToolInputShape::Json,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Graph-backed fast memory remember tool schema.
pub struct MemoryRememberTool;

#[async_trait]
impl BuiltInTool for MemoryRememberTool {
    fn name(&self) -> &'static str {
        "memory_remember"
    }

    fn description(&self) -> &'static str {
        "Synchronously remember a fact, decision, or lesson in graph memory."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Free-form fact text to remember." },
                "label": { "type": "string", "enum": ["Fact", "Decision", "Lesson", "Entity", "Concept", "Incident", "Source"], "default": "Fact" },
                "scope": { "type": "string", "enum": ["workspace", "user"], "default": "workspace" },
                "supersedes_specific": { "type": "string", "description": "Optional UUID of the graph node this fact explicitly supersedes." }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        fast_memory_policy()
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::NonIdempotent
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        moa_memory_ingest::execute_memory_tool(ctx.session, self.name(), input).await
    }
}

/// Graph-backed fast memory forget tool schema.
pub struct MemoryForgetTool;

#[async_trait]
impl BuiltInTool for MemoryForgetTool {
    fn name(&self) -> &'static str {
        "memory_forget"
    }

    fn description(&self) -> &'static str {
        "Synchronously soft-forget graph memory by node UUID, exact projected name, or all active user-scoped nodes for a user."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Exact graph node UUID to soft-invalidate." },
                "name": { "type": "string", "description": "Exact projected node name to soft-invalidate." },
                "soft_all_user_id": { "type": "string", "description": "User UUID whose active user-scoped nodes should be soft-invalidated." }
            },
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        fast_memory_policy()
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::NonIdempotent
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        moa_memory_ingest::execute_memory_tool(ctx.session, self.name(), input).await
    }
}

/// Graph-backed fast memory supersede tool schema.
pub struct MemorySupersedeTool;

#[async_trait]
impl BuiltInTool for MemorySupersedeTool {
    fn name(&self) -> &'static str {
        "memory_supersede"
    }

    fn description(&self) -> &'static str {
        "Synchronously replace an existing graph memory node with a new fact."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "old_uid": { "type": "string", "description": "UUID of the active node being superseded." },
                "new_text": { "type": "string", "description": "Replacement fact text." },
                "label": { "type": "string", "enum": ["Fact", "Decision", "Lesson", "Entity", "Concept", "Incident", "Source"], "default": "Fact" },
                "scope": { "type": "string", "enum": ["workspace", "user"], "default": "workspace" }
            },
            "required": ["old_uid", "new_text"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        fast_memory_policy()
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::NonIdempotent
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        moa_memory_ingest::execute_memory_tool(ctx.session, self.name(), input).await
    }
}
