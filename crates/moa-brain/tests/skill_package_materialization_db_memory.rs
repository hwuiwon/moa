//! End-to-end skill package materialization coverage for the brain turn loop.

use std::sync::Arc;

use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_brain::{
    GraphMemoryPipelineOptions, TurnResult,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions, run_brain_turn,
};
use moa_core::{
    ActionRuleScope, AgentContext, AgentPolicySnapshot, AgentRevisionLock, AgentSkillPolicy,
    AgentSkillPolicyMode, AgentToolPolicy, AgentToolPolicyMode, ContactId, ContactRef,
    ContactVerificationState, Event, EventRange, LockedToolRef, ModelCapabilities, ModelId,
    ResolvedArtifactRevisionRef, Result, SessionActorRef, SessionMeta, SessionStore, TenantId,
    TokenPricing, ToolCallFormat, ToolOutput, UserId, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_providers::{ScriptedBlock, ScriptedProvider, ScriptedResponse};
use moa_security::ActionPolicies;
use moa_session::testing;
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn db_backed_selected_skill_package_is_materialized_before_first_tool_call() -> Result<()> {
    // Pins: a DB-selected skill package is lazily installed and visible to file_read/bash tools.
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;

    let mut config = moa_core::MoaConfig::default();
    config.models.main = "claude-sonnet-4-6".to_string();
    config.memory.auto_bootstrap = false;
    config.skill_budget.max_manifest_chars = Some(512);

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let graph_pool = session_store.pool().clone();
    let session_store: Arc<dyn SessionStore> = Arc::new(session_store);
    let workspace_id = WorkspaceId::new(format!(
        "skill-package-materialization-{}",
        Uuid::now_v7().simple()
    ));
    let tenant_id = tenant_id_from_workspace_id(&workspace_id);
    let runtime_workspace_id = WorkspaceId::new(tenant_id.to_string());
    let user_id = UserId::new("skill-package-user");
    let skill_name = format!("db-backed-package-{}", Uuid::now_v7().simple());
    let session_id = session_store
        .create_session(session_meta(
            tenant_id,
            &user_id,
            config.models.main.clone().into(),
            None,
        ))
        .await?;

    let skill_document = skill_artifact_document(&skill_name);
    let skill_source = skill_document.to_yaml().expect("serialize skill artifact");
    let skill_files = skill_files();
    let skill_draft = ArtifactRegistry::new(graph_pool.clone())
        .create_draft(
            &ActionRuleScope::Tenant { tenant_id },
            NewArtifactDraft {
                document: &skill_document,
                source_format: "yaml",
                source_text: skill_source.as_bytes(),
                files: &skill_files,
            },
        )
        .await?;
    ArtifactRegistry::new(graph_pool.clone())
        .publish_revision(
            &ActionRuleScope::Tenant { tenant_id },
            skill_draft.revision_uid,
            &validate_for_status(&skill_document, ArtifactStatus::Published),
        )
        .await?;

    let router = Arc::new(
        ToolRouter::new_local(&workspace)
            .await?
            .with_policies(ActionPolicies::from_config(&config)),
    );
    router
        .remember_workspace_root(runtime_workspace_id, workspace.clone())
        .await;

    let provider = Arc::new(scripted_provider(&skill_name));
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        session_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool,
            shared_graph_memory_retriever: None,
            retrieval_embedder: None,
            shared_skill_injector: None,
            compaction_llm_provider: None,
            query_rewrite_llm_provider: None,
            discovered_workspace_instructions: None,
            tool_schemas: router.tool_schemas(),
            lineage: Arc::new(moa_core::NullLineageHandle),
        },
    );
    session_store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: format!("Use the {skill_name} package skill helper and checklist"),
                attachments: Vec::new(),
            },
        )
        .await?;

    let result = run_brain_turn(
        session_id,
        session_store.clone(),
        provider.clone(),
        &pipeline,
        Some(router),
    )
    .await?;
    assert_eq!(result, TurnResult::Complete);

    let requests = provider.recorded_requests();
    assert_eq!(requests.len(), 2, "tool turn plus final response");
    assert!(
        !requests[0]
            .metadata
            .contains_key("selected_skill_sandbox_files"),
        "selected package bytes must not enter provider metadata"
    );
    assert_eq!(
        requests[0].metadata["selected_skill_sandbox_file_count"],
        json!(3)
    );

    let events = session_store
        .get_events(session_id, EventRange::all())
        .await?;
    let tool_results = tool_results_by_provider_id(&events);
    assert_eq!(tool_results.len(), 3);
    assert_eq!(
        tool_results[0].0.as_deref(),
        Some("read_skill_md"),
        "first tool should read SKILL.md"
    );
    assert!(tool_results[0].1.to_text().contains("Run the helper"));
    assert_eq!(tool_results[1].0.as_deref(), Some("read_checklist"));
    assert_eq!(
        tool_results[1].1.to_text(),
        "Checklist item: verify package materialization."
    );
    assert_eq!(tool_results[2].0.as_deref(), Some("run_script"));
    assert_eq!(
        tool_results[2].1.process_stdout(),
        Some("helper-script-ok\n")
    );
    assert_eq!(tool_results[2].1.process_exit_code(), Some(0));

    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "Package materialized."
    )));

    testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn agent_locked_skill_revision_materializes_exact_files_after_newer_publish() -> Result<()> {
    // Pins: configured-agent sessions install the skill revision locked by the agent, not latest.
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;

    let mut config = moa_core::MoaConfig::default();
    config.models.main = "claude-sonnet-4-6".to_string();
    config.memory.auto_bootstrap = false;
    config.skill_budget.max_manifest_chars = Some(512);

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let graph_pool = session_store.pool().clone();
    let session_store: Arc<dyn SessionStore> = Arc::new(session_store);
    let workspace_id = WorkspaceId::new(format!("agent-locked-skill-{}", Uuid::now_v7().simple()));
    let tenant_id = tenant_id_from_workspace_id(&workspace_id);
    let runtime_workspace_id = WorkspaceId::new(tenant_id.to_string());
    let user_id = UserId::new("agent-locked-skill-user");
    let skill_name = format!("agent-locked-skill-{}", Uuid::now_v7().simple());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let registry = ArtifactRegistry::new(graph_pool.clone());

    let skill_document = skill_artifact_document(&skill_name);
    let skill_source = skill_document.to_yaml().expect("serialize skill artifact");
    let first_draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &skill_document,
                source_format: "yaml",
                source_text: skill_source.as_bytes(),
                files: &skill_files_with_checklist("Pinned v1 checklist.\n"),
            },
        )
        .await?;
    let first_revision = registry
        .publish_revision(
            &scope,
            first_draft.revision_uid,
            &validate_for_status(&skill_document, ArtifactStatus::Published),
        )
        .await?;
    let second_draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &skill_document,
                source_format: "yaml",
                source_text: skill_source.as_bytes(),
                files: &skill_files_with_checklist("Latest v2 checklist.\n"),
            },
        )
        .await?;
    registry
        .publish_revision(
            &scope,
            second_draft.revision_uid,
            &validate_for_status(&skill_document, ArtifactStatus::Published),
        )
        .await?;
    let agent_document = agent_artifact_document(
        &format!("locked-agent-{}", Uuid::now_v7().simple()),
        &skill_name,
    );
    let agent_source = agent_document.to_yaml().expect("serialize agent artifact");
    let agent_draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &agent_document,
                source_format: "yaml",
                source_text: agent_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    let agent_revision = registry
        .publish_revision(
            &scope,
            agent_draft.revision_uid,
            &validate_for_status(&agent_document, ArtifactStatus::Published),
        )
        .await?;

    let session_id = session_store
        .create_session(session_meta(
            tenant_id,
            &user_id,
            config.models.main.clone().into(),
            Some(agent_context_with_skill_revision(
                &skill_name,
                &first_revision,
                agent_revision.revision_uid,
            )),
        ))
        .await?;

    let router = Arc::new(
        ToolRouter::new_local(&workspace)
            .await?
            .with_policies(ActionPolicies::from_config(&config)),
    );
    router
        .remember_workspace_root(runtime_workspace_id, workspace.clone())
        .await;

    let provider = Arc::new(scripted_provider_read_checklist(&skill_name));
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        session_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool,
            shared_graph_memory_retriever: None,
            retrieval_embedder: None,
            shared_skill_injector: None,
            compaction_llm_provider: None,
            query_rewrite_llm_provider: None,
            discovered_workspace_instructions: None,
            tool_schemas: router.tool_schemas(),
            lineage: Arc::new(moa_core::NullLineageHandle),
        },
    );
    session_store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: format!("Use the {skill_name} package checklist"),
                attachments: Vec::new(),
            },
        )
        .await?;

    let result = run_brain_turn(
        session_id,
        session_store.clone(),
        provider.clone(),
        &pipeline,
        Some(router),
    )
    .await?;
    assert_eq!(result, TurnResult::Complete);

    let events = session_store
        .get_events(session_id, EventRange::all())
        .await?;
    let tool_results = tool_results_by_provider_id(&events);
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].0.as_deref(), Some("read_checklist"));
    assert_eq!(tool_results[0].1.to_text(), "Pinned v1 checklist.");

    testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn skill_files() -> Vec<NewArtifactFile> {
    skill_files_with_checklist("Checklist item: verify package materialization.\n")
}

fn skill_files_with_checklist(checklist: &str) -> Vec<NewArtifactFile> {
    vec![
        NewArtifactFile {
            path: "SKILL.md".to_string(),
            content: skill_markdown().as_bytes().to_vec(),
            content_type: Some("text/markdown; charset=utf-8".to_string()),
            executable: false,
        },
        NewArtifactFile {
            path: "references/checklist.md".to_string(),
            content: checklist.as_bytes().to_vec(),
            content_type: Some("text/markdown; charset=utf-8".to_string()),
            executable: false,
        },
        NewArtifactFile {
            path: "scripts/run.sh".to_string(),
            content: b"printf 'helper-script-ok\n'".to_vec(),
            content_type: Some("text/x-shellscript".to_string()),
            executable: true,
        },
    ]
}

fn agent_context_with_skill_revision(
    skill_name: &str,
    revision: &moa_artifacts::registry::StoredArtifactRevision,
    agent_revision_uid: Uuid,
) -> AgentContext {
    let dependency = ResolvedArtifactRevisionRef {
        reference: format!("skill://{skill_name}"),
        kind: "skill".to_string(),
        name: skill_name.to_string(),
        artifact_uid: revision.artifact_uid,
        revision_uid: revision.revision_uid,
        version: revision.version,
    };
    let lock = AgentRevisionLock {
        agent_revision_uid,
        artifact_dependencies: vec![dependency.clone()],
        tool_dependencies: vec![LockedToolRef {
            name: "file_read".to_string(),
            schema_hash: "file-read-schema".to_string(),
            provider: None,
        }],
        canonical_policy_hash: "locked-skill-policy".to_string(),
    };
    let snapshot = AgentPolicySnapshot {
        instructions: vec![format!("Use only {skill_name} for package guidance.")],
        skill_policy: AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Pinned,
            refs: vec![format!("skill://{skill_name}")],
            max_visible: None,
        },
        tool_policy: AgentToolPolicy {
            mode: AgentToolPolicyMode::Allowlist,
            tools: vec!["file_read".to_string()],
            denied_tools: Vec::new(),
        },
        revision_lock: Some(lock),
        ..AgentPolicySnapshot::default()
    };
    AgentContext {
        agent_id: None,
        installation_uid: None,
        deployment_uid: None,
        definition_ref: "agent://locked-skill-test".to_string(),
        revision_uid: agent_revision_uid,
        policy_hash: "locked-skill-policy".to_string(),
        display_name: "Locked Skill Test".to_string(),
        artifact_dependencies: vec![dependency],
        tool_dependencies: vec![LockedToolRef {
            name: "file_read".to_string(),
            schema_hash: "file-read-schema".to_string(),
            provider: None,
        }],
        policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
    }
}

fn skill_artifact_document(skill_name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": skill_name,
            "description": "DB-backed package materialization fixture",
            "tags": ["package", "materialization"]
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "allowed_tools": ["file_read", "bash"]
            }
        }
    }))
    .expect("skill artifact fixture is valid")
}

fn agent_artifact_document(agent_name: &str, skill_name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "agent",
        "metadata": {
            "name": agent_name,
            "description": "Locked skill test agent"
        },
        "definition": {
            "type": "agent",
            "spec": {
                "display_name": "Locked Skill Test",
                "purpose": {
                    "summary": "Exercise pinned skill materialization.",
                    "expected_outputs": ["checklist"]
                },
                "instruction_policy": {
                    "instructions": ["Use the pinned skill package."]
                },
                "skill_policy": {
                    "mode": "pinned",
                    "refs": [format!("skill://{skill_name}")]
                },
                "tool_policy": {
                    "mode": "allowlist",
                    "tools": ["file_read"]
                }
            }
        }
    }))
    .expect("agent artifact fixture is valid")
}

fn skill_markdown() -> &'static str {
    r#"---
name: db-backed-package
description: "DB-backed package materialization fixture"
allowed-tools: file_read bash
metadata:
  moa-tags: "package, materialization"
  moa-use-count: "10"
  moa-estimated-tokens: "80"
---

# DB-backed Package

Run the helper script and read the checklist when package materialization is requested.
"#
}

fn scripted_provider(skill_name: &str) -> ScriptedProvider {
    let skill_base = format!(".moa/skills/{}", slugify_skill_name(skill_name));
    ScriptedProvider::new(capabilities())
        .push_response(ScriptedResponse::from_blocks(vec![
            ScriptedBlock::tool_call(
                "file_read",
                json!({ "path": format!("{skill_base}/SKILL.md") }),
                "read_skill_md",
            ),
            ScriptedBlock::tool_call(
                "file_read",
                json!({ "path": format!("{skill_base}/references/checklist.md") }),
                "read_checklist",
            ),
            ScriptedBlock::tool_call(
                "bash",
                json!({ "cmd": format!("{skill_base}/scripts/run.sh") }),
                "run_script",
            ),
        ]))
        .push_response(ScriptedResponse::text("Package materialized."))
}

fn scripted_provider_read_checklist(skill_name: &str) -> ScriptedProvider {
    let skill_base = format!(".moa/skills/{}", slugify_skill_name(skill_name));
    ScriptedProvider::new(capabilities())
        .push_response(ScriptedResponse::from_blocks(vec![
            ScriptedBlock::tool_call(
                "file_read",
                json!({ "path": format!("{skill_base}/references/checklist.md") }),
                "read_checklist",
            ),
        ]))
        .push_response(ScriptedResponse::text("Pinned package materialized."))
}

fn slugify_skill_name(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: moa_core::ModelId::new("claude-sonnet-4-6"),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

fn session_meta(
    tenant_id: TenantId,
    user_id: &UserId,
    model: ModelId,
    agent_context: Option<AgentContext>,
) -> SessionMeta {
    let contact_id = contact_id_from_user_id(user_id);
    SessionMeta {
        tenant_id,
        contact: Some(contact_ref(tenant_id, contact_id)),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model,
        agent_context,
        ..SessionMeta::default()
    }
}

fn tenant_id_from_workspace_id(workspace_id: &WorkspaceId) -> TenantId {
    Uuid::parse_str(workspace_id.as_str())
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(workspace_id.as_str())))
}

fn contact_id_from_user_id(user_id: &UserId) -> ContactId {
    Uuid::parse_str(user_id.as_str())
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id.as_str())))
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let hash = blake3::hash(label.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}

fn tool_results_by_provider_id(
    events: &[moa_core::EventRecord],
) -> Vec<(Option<String>, ToolOutput)> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolResult {
                provider_tool_use_id,
                output,
                ..
            } => Some((provider_tool_use_id.clone(), output.clone())),
            _ => None,
        })
        .collect()
}
