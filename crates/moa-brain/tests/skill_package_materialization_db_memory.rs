//! End-to-end skill package materialization coverage for the brain turn loop.

use std::sync::Arc;

use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_brain::{
    BrainTurnRequest, DigestStageInput, GraphMemoryPipelineStages, GraphMemoryStageInput,
    HistoryStageInput, QueryRewriteStageInput, RuntimeStageInput, SkillInjectionStageInput,
    TurnResult, build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    run_brain_turn,
};
use moa_core::{
    error::Result, events::Event, traits::SessionStore, types::action_policy::ActionRuleScope,
    types::agent::AgentContext, types::agent::AgentPolicySnapshot, types::agent::AgentRevisionLock,
    types::agent::AgentSkillPolicy, types::agent::AgentSkillPolicyMode,
    types::agent::AgentToolPolicy, types::agent::AgentToolPolicyMode, types::agent::LockedToolRef,
    types::agent::ResolvedArtifactRevisionRef, types::contact::ContactId,
    types::contact::ContactRef, types::contact::ContactVerificationState,
    types::contact::SessionActorRef, types::events_stream::EventRange, types::identifiers::ModelId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::UserId, types::model::ModelCapabilities, types::model::TokenPricing,
    types::model::ToolCallFormat, types::session::SessionMeta, types::tools::ToolOutput,
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

    let mut config = moa_config::MoaConfig::default();
    config.models.main = "claude-sonnet-4-6".to_string();
    // The manifest budget must clear the fixed preamble/footer (~550 chars, now that
    // each entry carries the exact [activate: <path>] activation guidance) plus one
    // package skill entry; the earlier 512 left no room for a single entry, starving
    // selection to zero. Sized generously so this test exercises materialization, not
    // budget starvation.
    config.skill_budget.max_manifest_chars = Some(2048);

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let graph_pool = session_store.pool().clone();
    let session_store = Arc::new(session_store);
    let dyn_session_store: Arc<dyn SessionStore> = session_store.clone();
    let storage_partition_id = StoragePartitionId::new(format!(
        "skill-package-materialization-{}",
        Uuid::now_v7().simple()
    ));
    let tenant_id = tenant_id_from_storage_partition_id(&storage_partition_id);
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
    allow_skill_package_bash(&session_store, tenant_id).await?;

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
    serve_skill_revision(&graph_pool, tenant_id, &skill_draft).await?;

    let router = Arc::new(
        ToolRouter::new_local(&workspace)
            .await?
            .with_policies(ActionPolicies::from_config(&config)?)
            .with_rule_store(session_store.clone()),
    );
    router
        .remember_workspace_root(tenant_id, workspace.clone())
        .await;

    let provider = Arc::new(scripted_provider(&skill_name));
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        dyn_session_store.clone(),
        GraphMemoryPipelineStages {
            history: HistoryStageInput {
                compaction_llm_provider: None,
            },
            graph_memory: GraphMemoryStageInput::Local {
                graph_pool: graph_pool.clone(),
                kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
                retrieval_embedder: None,
                lineage: Arc::new(moa_core::traits::NullLineageHandle),
            },
            skill_injection: SkillInjectionStageInput::Local {
                graph_pool: graph_pool.clone(),
                segment_store: None,
                embedder: None,
            },
            query_rewrite: QueryRewriteStageInput { llm_provider: None },
            runtime: RuntimeStageInput {
                identity_prompt_override: None,
                tool_schemas: router.tool_schemas(),
            },
            digest: DigestStageInput {
                graph_pool: graph_pool.clone(),
            },
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

    let result = run_brain_turn(BrainTurnRequest {
        identity: test_identity(tenant_id),
        session_id,
        session_store: dyn_session_store.clone(),
        llm_provider: provider.clone(),
        pipeline: &pipeline,
        tool_router: Some(router),
        workspace_scope: Some(
            moa_core::types::sandbox_workspace::SandboxWorkspaceScope::Worker {
                session_id,
                worker_id: "skill-package-materialization-worker".to_string(),
            },
        ),
    })
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
        Some("helper-script-ok\n"),
        "materialized helper result was: {:#?}",
        tool_results[2].1,
    );
    assert_eq!(tool_results[2].1.process_exit_code(), Some(0));

    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "Package materialized."
    )));

    testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn agent_locked_superseded_skill_revision_materializes_exact_files_after_newer_activation()
-> Result<()> {
    // Pins: configured-agent sessions install their exact historically activated
    // skill revision after a newer activation supersedes it.
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;

    let mut config = moa_config::MoaConfig::default();
    config.models.main = "claude-sonnet-4-6".to_string();
    // The manifest budget must clear the fixed preamble/footer (~550 chars, now that
    // each entry carries the exact [activate: <path>] activation guidance) plus one
    // package skill entry; the earlier 512 left no room for a single entry, starving
    // selection to zero. Sized generously so this test exercises materialization, not
    // budget starvation.
    config.skill_budget.max_manifest_chars = Some(2048);

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let graph_pool = session_store.pool().clone();
    let session_store: Arc<dyn SessionStore> = Arc::new(session_store);
    let storage_partition_id =
        StoragePartitionId::new(format!("agent-locked-skill-{}", Uuid::now_v7().simple()));
    let tenant_id = tenant_id_from_storage_partition_id(&storage_partition_id);
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
    serve_skill_revision(&graph_pool, tenant_id, &first_draft).await?;
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
    serve_skill_revision(&graph_pool, tenant_id, &second_draft).await?;
    let first_revision = registry
        .load_revision(&scope, first_draft.revision_uid)
        .await?
        .expect("first skill revision remains available after supersession");
    assert_eq!(first_revision.status, ArtifactStatus::Superseded);
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
    registry
        .record_validation_report(
            &scope,
            agent_draft.revision_uid,
            &validate_for_status(&agent_document, ArtifactStatus::Ready),
        )
        .await?;
    let agent_revision = registry
        .load_revision(&scope, agent_draft.revision_uid)
        .await?
        .expect("validated agent candidate remains loadable");

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
            .with_policies(ActionPolicies::from_config(&config)?),
    );
    router
        .remember_workspace_root(tenant_id, workspace.clone())
        .await;

    let provider = Arc::new(scripted_provider_read_checklist(&skill_name));
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        session_store.clone(),
        GraphMemoryPipelineStages {
            history: HistoryStageInput {
                compaction_llm_provider: None,
            },
            graph_memory: GraphMemoryStageInput::Local {
                graph_pool: graph_pool.clone(),
                kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
                retrieval_embedder: None,
                lineage: Arc::new(moa_core::traits::NullLineageHandle),
            },
            skill_injection: SkillInjectionStageInput::Local {
                graph_pool: graph_pool.clone(),
                segment_store: None,
                embedder: None,
            },
            query_rewrite: QueryRewriteStageInput { llm_provider: None },
            runtime: RuntimeStageInput {
                identity_prompt_override: None,
                tool_schemas: router.tool_schemas(),
            },
            digest: DigestStageInput { graph_pool },
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

    let result = run_brain_turn(BrainTurnRequest {
        identity: test_identity(tenant_id),
        session_id,
        session_store: session_store.clone(),
        llm_provider: provider.clone(),
        pipeline: &pipeline,
        tool_router: Some(router),
        workspace_scope: Some(
            moa_core::types::sandbox_workspace::SandboxWorkspaceScope::Worker {
                session_id,
                worker_id: "skill-package-symlink-worker".to_string(),
            },
        ),
    })
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

fn test_identity(tenant_id: TenantId) -> moa_core::traits::Identity {
    moa_core::traits::Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c415),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

/// Opts this tenant into running the materialized `run.sh` helper without
/// weakening the production `AdminReview` default for `bash` command execution.
async fn allow_skill_package_bash(
    store: &moa_session::PostgresSessionStore,
    tenant_id: TenantId,
) -> Result<()> {
    store
        .upsert_action_policy_rule(moa_core::types::action_policy::ActionPolicyRule {
            id: Uuid::now_v7(),
            scope: ActionRuleScope::Tenant { tenant_id },
            tool: "bash".to_string(),
            pattern: "*run.sh".to_string(),
            effect: moa_core::types::action_policy::ActionPolicyEffect::Allow,
            reason: Some("skill package materialization test bash opt-in".to_string()),
            created_by: UserId::new("skill-package-test"),
            created_at: moa_test_support::fixtures::pg_now(),
        })
        .await
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
            content: b"#!/bin/sh\nprintf 'helper-script-ok\n'".to_vec(),
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
            identity_hash: "file-read-schema".to_string(),
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
            identity_hash: "file-read-schema".to_string(),
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
        model_id: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
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
        agent_context: Some(agent_context.unwrap_or_else(AgentContext::system_default)),
        ..SessionMeta::default()
    }
}

fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    Uuid::parse_str(storage_partition_id.as_str())
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id.as_str())))
}

fn contact_id_from_user_id(user_id: &UserId) -> ContactId {
    Uuid::parse_str(user_id.as_str())
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id.as_str())))
}

use moa_test_support::fixtures::stable_uuid_from_label;

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
    events: &[moa_core::types::events_stream::EventRecord],
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

/// Activates a skill revision so the tenant serves it.
///
/// A turn-loop test that needs the skill resolvable drives the release path. The
/// fixture runs the real submit, decide, and activate transaction.
async fn serve_skill_revision(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    revision: &moa_artifacts::registry::StoredArtifactRevision,
) -> Result<()> {
    moa_artifacts::test_fixtures::activate_revision(
        pool,
        moa_artifacts::release::TenantScope::new(tenant_id),
        moa_artifacts::release::ActivationTarget::SkillVisibility {
            artifact_uid: revision.artifact_uid,
        },
        revision.revision_uid,
    )
    .await
    .map_err(|error| moa_core::error::MoaError::ValidationError(error.to_string()))?;
    Ok(())
}
