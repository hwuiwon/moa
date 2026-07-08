//! Graph-backed built-in memory tool schemas.

use async_trait::async_trait;
use moa_core::{
    ActionClass, ActionPolicyEffect, BuiltInTool, IdempotencyClass, Result, RiskLevel, ToolContext,
    ToolDiffStrategy, ToolInputShape, ToolOutput, ToolPolicySpec,
};

/// Names of the read-only agentic memory retrieval tools.
///
/// These built-ins are registered so they can execute when the model calls
/// them, but they are deliberately kept out of the default prompt loadout: the
/// brain gates them onto a turn only when the router selects the agentic
/// strategy or the injected retrieval returned nothing (plan Task 11).
pub const AGENTIC_MEMORY_TOOL_NAMES: [&str; 2] = ["memory_search", "memory_navigate"];

fn fast_memory_policy() -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Medium,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::LocalWrite,
        input_shape: ToolInputShape::Json,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Read-only policy for the agentic memory retrieval tools.
fn memory_retrieval_policy() -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Low,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::Read,
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
                "scope": { "type": "string", "enum": ["tenant", "contact"], "default": "tenant" },
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
        execute_memory_tool(ctx, self.name(), input).await
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
        "Synchronously soft-forget graph memory by node UUID, exact projected name, or all active contact-scoped nodes for a contact."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Exact graph node UUID to soft-invalidate." },
                "name": { "type": "string", "description": "Exact projected node name to soft-invalidate." },
                "soft_all_user_id": { "type": "string", "description": "Contact UUID whose active contact-scoped nodes should be soft-invalidated." }
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
        execute_memory_tool(ctx, self.name(), input).await
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
                "scope": { "type": "string", "enum": ["tenant", "contact"], "default": "tenant" }
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
        execute_memory_tool(ctx, self.name(), input).await
    }
}

async fn execute_memory_tool(
    ctx: &ToolContext<'_>,
    tool_name: &str,
    input: &serde_json::Value,
) -> Result<ToolOutput> {
    let executor = ctx.memory_tool_executor.ok_or_else(|| {
        moa_core::MoaError::Unsupported(
            "graph-memory tools require a runtime memory executor".to_string(),
        )
    })?;
    executor
        .execute_memory_tool(ctx.session, tool_name, input)
        .await
}

/// Read-only graph-memory search tool for the agentic retrieval strategy.
///
/// Runs the same scoped hybrid retrieval the stage-7 injection path uses and
/// returns per-hit provenance so tool-derived answers stay citable. The RLS
/// scope is derived from the session by the installed executor; the tool never
/// accepts caller-supplied tenant or contact identifiers.
pub struct MemorySearchTool;

#[async_trait]
impl BuiltInTool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        "Search your own graph memory and tenant knowledge for facts relevant to a query. Returns ranked hits with provenance (graph_uid, chunk_uid, source_uri) that you can cite. Scoped automatically to the current session; you cannot search another tenant or contact."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language search query." },
                "scope": { "type": "string", "enum": ["auto"], "default": "auto", "description": "Retrieval scope. Only `auto` is supported; the session's tenant/contact scope is always used." }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        memory_retrieval_policy()
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::Idempotent
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        execute_memory_retrieval_tool(ctx, self.name(), input).await
    }
}

/// Read-only graph-memory navigation tool for the agentic retrieval strategy.
///
/// Walks one to three hops out from a known node under the session scope,
/// optionally filtered by edge label, so the model can follow relationships it
/// discovered via [`MemorySearchTool`].
pub struct MemoryNavigateTool;

#[async_trait]
impl BuiltInTool for MemoryNavigateTool {
    fn name(&self) -> &'static str {
        "memory_navigate"
    }

    fn description(&self) -> &'static str {
        "Walk the graph outward from a known memory node (by uid) to its neighbors, optionally filtered by edge label. Use after memory_search to follow relationships. Scoped automatically to the current session."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "node_uid": { "type": "string", "description": "UUID of the node to expand, typically a graph_uid returned by memory_search." },
                "edge_labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional edge labels to follow (e.g. CONTAINS, MENTIONED_IN, DERIVED_FROM). Omit to follow all edges."
                },
                "hops": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1, "description": "Number of hops to traverse (1-3)." }
            },
            "required": ["node_uid"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        memory_retrieval_policy()
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::Idempotent
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        execute_memory_retrieval_tool(ctx, self.name(), input).await
    }
}

async fn execute_memory_retrieval_tool(
    ctx: &ToolContext<'_>,
    tool_name: &str,
    input: &serde_json::Value,
) -> Result<ToolOutput> {
    let executor = ctx.memory_retrieval_executor.ok_or_else(|| {
        moa_core::MoaError::Unsupported(
            "graph-memory retrieval tools require a runtime retrieval executor".to_string(),
        )
    })?;
    executor
        .execute_retrieval_tool(ctx.session, tool_name, input)
        .await
}
