//! Unit coverage for the tool executor's idempotency and registry helpers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    BuiltInTool, Event, EventRecord, EventType, IdempotencyClass, ToolCallId, ToolCallRequest,
    ToolContext, ToolDefinition, ToolDiffStrategy, ToolInputShape, ToolOutput, ToolPolicySpec,
    UserId, WorkspaceId, read_tool_policy, write_tool_policy,
};
use moa_hands::{ToolRegistry, ToolRouter};
use moa_memory::FileMemoryStore;
use moa_orchestrator::services::tool_executor::{
    ToolDescriptor, ToolExecutorImpl, build_tool_run_plan, has_prior_non_idempotent_result,
    tool_run_name,
};
use serde_json::{Value, json};
use tempfile::tempdir;
use uuid::Uuid;

struct CountingTool {
    name: &'static str,
    idempotency_class: IdempotencyClass,
    policy: ToolPolicySpec,
}

impl CountingTool {
    fn new(
        name: &'static str,
        idempotency_class: IdempotencyClass,
        policy: ToolPolicySpec,
    ) -> Self {
        Self {
            name,
            idempotency_class,
            policy,
        }
    }
}

#[async_trait]
impl BuiltInTool for CountingTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "counting test tool"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": true
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        self.policy.clone()
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        self.idempotency_class
    }

    async fn execute(
        &self,
        _input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> moa_core::Result<ToolOutput> {
        Ok(ToolOutput::text(
            self.name,
            std::time::Duration::from_millis(1),
        ))
    }
}

fn registry_with_tools(tools: Vec<Arc<dyn BuiltInTool>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register_builtin(tool);
    }
    registry
}

fn tool_request(
    tool_call_id: ToolCallId,
    tool_name: &str,
    idempotency_key: Option<&str>,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id,
        provider_tool_use_id: None,
        tool_name: tool_name.to_string(),
        input: json!({}),
        session_id: None,
        workspace_id: WorkspaceId::new("workspace-1"),
        user_id: UserId::new("user-1"),
        idempotency_key: idempotency_key.map(ToOwned::to_owned),
    }
}

fn tool_definition(
    tool_name: &str,
    idempotency_class: IdempotencyClass,
    policy: ToolPolicySpec,
) -> ToolDefinition {
    ToolDefinition {
        name: tool_name.to_string(),
        description: "mock".to_string(),
        schema: json!({"type": "object"}),
        policy,
        idempotency_class,
        max_output_tokens: 8_000,
    }
}

fn tool_result_record(tool_call_id: ToolCallId) -> EventRecord {
    EventRecord {
        id: Uuid::now_v7(),
        session_id: moa_core::SessionId::new(),
        sequence_num: 0,
        event_type: EventType::ToolResult,
        event: Event::ToolResult {
            tool_id: tool_call_id,
            provider_tool_use_id: None,
            output: ToolOutput::text("stored", std::time::Duration::from_millis(1)),
            original_output_tokens: None,
            success: true,
            duration_ms: 1,
        },
        timestamp: Utc::now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    }
}

#[test]
fn idempotent_tool_retries_freely() {
    let definition = tool_definition(
        "mock_read",
        IdempotencyClass::Idempotent,
        read_tool_policy(ToolInputShape::Json),
    );
    let request = tool_request(ToolCallId::new(), "mock_read", None);

    let run_plan = build_tool_run_plan(&definition, &request).expect("build idempotent run plan");

    assert_eq!(run_plan.max_attempts, 3);
    assert_eq!(
        run_plan.name,
        tool_run_name(&definition, &request).expect("build idempotent run name")
    );
}

#[test]
fn non_idempotent_refuses_after_event_log_hit() {
    let tool_call_id = ToolCallId::new();
    let records = vec![tool_result_record(tool_call_id)];

    assert!(has_prior_non_idempotent_result(&records, tool_call_id));
    assert!(!has_prior_non_idempotent_result(
        &records,
        ToolCallId::new(),
    ));
}

#[test]
fn keyed_tool_requires_idempotency_key() {
    let definition = tool_definition(
        "mock_keyed",
        IdempotencyClass::IdempotentWithKey,
        read_tool_policy(ToolInputShape::Json),
    );
    let request = tool_request(ToolCallId::new(), "mock_keyed", None);

    let error = build_tool_run_plan(&definition, &request)
        .expect_err("keyed tools should reject missing idempotency keys");

    assert!(error.to_string().contains("requires idempotency_key"));
}

#[test]
fn run_name_encodes_tool_call_id() {
    let tool_call_id = ToolCallId::new();
    let definition = tool_definition(
        "mock_read",
        IdempotencyClass::Idempotent,
        read_tool_policy(ToolInputShape::Json),
    );
    let request = tool_request(tool_call_id, "mock_read", None);

    let run_name = tool_run_name(&definition, &request).expect("build run name");

    assert!(run_name.contains(&tool_call_id.to_string()));
    assert!(run_name.starts_with("tool_execute:idempotent:mock_read:"));
}

#[tokio::test]
async fn list_tools_returns_workspace_tools() {
    let memory_root = tempdir().expect("create temporary memory root");
    let memory_store = Arc::new(
        FileMemoryStore::new(memory_root.path())
            .await
            .expect("create file memory store"),
    );
    let registry = registry_with_tools(vec![
        Arc::new(CountingTool::new(
            "read_tool",
            IdempotencyClass::Idempotent,
            read_tool_policy(ToolInputShape::Json),
        )),
        Arc::new(CountingTool::new(
            "write_tool",
            IdempotencyClass::NonIdempotent,
            write_tool_policy(ToolInputShape::Json, ToolDiffStrategy::None),
        )),
    ]);
    let router = Arc::new(ToolRouter::new(registry, memory_store, HashMap::new()));
    let executor = ToolExecutorImpl::new(router);

    let descriptors = executor.list_descriptors();

    assert!(descriptors.iter().any(|descriptor: &ToolDescriptor| {
        descriptor.name == "read_tool"
            && descriptor.idempotency_class == IdempotencyClass::Idempotent
            && !descriptor.requires_approval
    }));
    assert!(descriptors.iter().any(|descriptor: &ToolDescriptor| {
        descriptor.name == "write_tool"
            && descriptor.idempotency_class == IdempotencyClass::NonIdempotent
            && descriptor.requires_approval
    }));
}
