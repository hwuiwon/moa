//! Unit coverage for the tool executor's idempotency and registry helpers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    events::Event, events::EventType, traits::BuiltInTool, traits::Identity, traits::IdentityType,
    traits::ToolContext, types::action_policy::ActionClass, types::action_policy::RiskLevel,
    types::events_stream::EventRecord, types::hands::SandboxFile, types::identifiers::SessionId,
    types::identifiers::TenantId, types::identifiers::ToolCallId, types::tools::IdempotencyClass,
    types::tools::ToolCallRequest, types::tools::ToolDefinition, types::tools::ToolDiffStrategy,
    types::tools::ToolInputShape, types::tools::ToolOutput, types::tools::ToolPolicySpec,
    types::tools::TrustedSandboxFileEntry, types::tools::TrustedSandboxFileManifestPayload,
    types::tools::TrustedSandboxFileManifestRef, types::tools::read_tool_policy,
    types::tools::write_tool_policy,
};
use moa_hands::{ToolRegistry, ToolRouter};
use moa_orchestrator::services::tool_executor::{
    ToolExecutorImpl, build_tool_run_plan, has_prior_non_idempotent_result, tool_run_name,
    trusted_sandbox_files_from_manifest_payload,
};
use moa_wire::tools::ToolDescriptor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
    ) -> moa_core::error::Result<ToolOutput> {
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

fn tool_request(tool_call_id: ToolCallId, tool_name: &str) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id,
        caller_identity: Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(2),
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        provider_tool_use_id: None,
        tool_name: tool_name.to_string(),
        input: json!({}),
        active_canary: None,
        session_id: SessionId::new(),
        trusted_sandbox_manifest: None,
        worker_id: None,
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
        session_id: moa_core::types::identifiers::SessionId::new(),
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
        timestamp: moa_test_support::fixtures::pg_now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn trusted_manifest_for_payload(
    payload_text: &str,
    files: &[SandboxFile],
) -> TrustedSandboxFileManifestRef {
    TrustedSandboxFileManifestRef {
        blob_id: "blob://trusted-files".to_string(),
        size: payload_text.len(),
        manifest_sha256: sha256_hex(payload_text.as_bytes()),
        files: files
            .iter()
            .map(|file| TrustedSandboxFileEntry {
                path: file.path.clone(),
                content_sha256: sha256_hex(&file.content),
                size: file.content.len(),
                executable: file.executable,
            })
            .collect(),
    }
}

#[test]
fn trusted_sandbox_manifest_payload_rehydrates_files() {
    // Pins: trusted file request journals carry a durable manifest reference, not raw bytes.
    let files = vec![SandboxFile {
        path: ".moa/skills/search/SKILL.md".to_string(),
        content: b"trusted skill".to_vec(),
        executable: false,
    }];
    let payload = TrustedSandboxFileManifestPayload {
        files: files.clone(),
    };
    let payload_text = serde_json::to_string(&payload).expect("serialize trusted files payload");
    let manifest = trusted_manifest_for_payload(&payload_text, &files);

    let loaded = trusted_sandbox_files_from_manifest_payload(&manifest, &payload_text)
        .expect("valid manifest should rehydrate trusted files");

    assert_eq!(loaded, files);
}

#[test]
fn trusted_sandbox_manifest_payload_rejects_hash_mismatch() {
    // Pins: stale or tampered session blob content does not install trusted sandbox files.
    let files = vec![SandboxFile {
        path: ".moa/skills/search/SKILL.md".to_string(),
        content: b"trusted skill".to_vec(),
        executable: false,
    }];
    let payload = TrustedSandboxFileManifestPayload {
        files: files.clone(),
    };
    let mut payload_text =
        serde_json::to_string(&payload).expect("serialize trusted files payload");
    let manifest = trusted_manifest_for_payload(&payload_text, &files);
    payload_text.push(' ');

    let error = trusted_sandbox_files_from_manifest_payload(&manifest, &payload_text)
        .expect_err("tampered manifest payload should be rejected");

    assert!(
        error
            .to_string()
            .contains("trusted sandbox file manifest blob://trusted-files hash mismatch"),
        "error should name the manifest hash failure: {error}"
    );
}

#[test]
fn build_tool_run_plan_uses_max_attempts_one_for_idempotent_tools() {
    let definition = tool_definition(
        "mock_read",
        IdempotencyClass::Idempotent,
        read_tool_policy(ToolInputShape::Json),
    );
    let request = tool_request(ToolCallId::new(), "mock_read");

    let run_plan = build_tool_run_plan(&definition, &request).expect("build idempotent run plan");

    assert_eq!(run_plan.max_attempts, 1);
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
fn run_name_encodes_tool_call_id() {
    let tool_call_id = ToolCallId::new();
    let definition = tool_definition(
        "mock_read",
        IdempotencyClass::Idempotent,
        read_tool_policy(ToolInputShape::Json),
    );
    let request = tool_request(tool_call_id, "mock_read");

    let run_name = tool_run_name(&definition, &request).expect("build run name");

    assert!(run_name.contains(&tool_call_id.to_string()));
    assert!(run_name.starts_with("tool_execute:idempotent:mock_read:"));
}

#[tokio::test]
async fn list_tools_returns_workspace_tools() {
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
    let router = Arc::new(ToolRouter::new(registry, HashMap::new()));
    let executor = ToolExecutorImpl::new(router);

    let descriptors = executor.list_descriptors();

    assert!(descriptors.iter().any(|descriptor: &ToolDescriptor| {
        descriptor.name == "read_tool"
            && descriptor.idempotency_class == IdempotencyClass::Idempotent
            && descriptor.action_class == ActionClass::Read
            && descriptor.risk_level == RiskLevel::Low
    }));
    assert!(descriptors.iter().any(|descriptor: &ToolDescriptor| {
        descriptor.name == "write_tool"
            && descriptor.idempotency_class == IdempotencyClass::NonIdempotent
            && descriptor.action_class == ActionClass::LocalWrite
            && descriptor.risk_level == RiskLevel::Medium
    }));
}
