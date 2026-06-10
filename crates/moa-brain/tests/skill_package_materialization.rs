//! End-to-end skill package materialization coverage for the brain turn loop.

use std::sync::Arc;

use moa_brain::{
    GraphMemoryPipelineOptions, TurnResult,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    run_brain_turn_with_tools,
};
use moa_core::{
    Event, EventRange, MemoryScope, ModelCapabilities, Result, SessionMeta, SessionStore,
    TokenPricing, ToolCallFormat, ToolOutput, UserId, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_providers::{ScriptedBlock, ScriptedProvider, ScriptedResponse};
use moa_security::ToolPolicies;
use moa_session::testing;
use moa_skills::{NewSkill, SkillPackage, SkillPackageFile, SkillRegistry};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn db_backed_selected_skill_package_is_materialized_before_first_tool_call() -> Result<()> {
    // Pins: a DB-selected skill package is lazily installed and visible to file_read/bash tools.
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;

    let mut config = moa_core::MoaConfig::default();
    config.models.main = "claude-sonnet-4-6".to_string();
    config.memory.auto_bootstrap = false;
    config.permissions.auto_approve = vec!["bash".to_string()];

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let graph_pool = session_store.pool().clone();
    let session_store: Arc<dyn SessionStore> = Arc::new(session_store);
    let workspace_id = WorkspaceId::new("skill-package-materialization");
    let user_id = UserId::new("skill-package-user");
    let session_id = session_store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id,
            model: config.models.main.clone().into(),
            ..SessionMeta::default()
        })
        .await?;

    SkillRegistry::new(graph_pool.clone())
        .upsert_by_name(NewSkill::from_package(
            MemoryScope::Workspace {
                workspace_id: workspace_id.clone(),
            },
            skill_package(),
        ))
        .await?;

    let router = Arc::new(
        ToolRouter::new_local(&workspace)
            .await?
            .with_policies(ToolPolicies::from_config(&config)),
    );
    router
        .remember_workspace_root(workspace_id.clone(), workspace.clone())
        .await;

    let provider = Arc::new(scripted_provider());
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        session_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool,
            shared_graph_memory_retriever: None,
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
                text: "Use the package skill helper and checklist".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    let result = run_brain_turn_with_tools(
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

fn skill_package() -> SkillPackage {
    SkillPackage::new(vec![
        SkillPackageFile::new("SKILL.md", skill_markdown().as_bytes().to_vec())
            .with_content_type("text/markdown; charset=utf-8"),
        SkillPackageFile::new(
            "references/checklist.md",
            b"Checklist item: verify package materialization.\n".to_vec(),
        )
        .with_content_type("text/markdown; charset=utf-8"),
        SkillPackageFile::new("scripts/run.sh", b"printf 'helper-script-ok\n'".to_vec())
            .with_content_type("text/x-shellscript")
            .with_executable(true),
    ])
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

fn scripted_provider() -> ScriptedProvider {
    ScriptedProvider::new(capabilities())
        .push_response(ScriptedResponse::from_blocks(vec![
            ScriptedBlock::tool_call(
                "file_read",
                json!({ "path": ".moa/skills/db-backed-package/SKILL.md" }),
                "read_skill_md",
            ),
            ScriptedBlock::tool_call(
                "file_read",
                json!({ "path": ".moa/skills/db-backed-package/references/checklist.md" }),
                "read_checklist",
            ),
            ScriptedBlock::tool_call(
                "bash",
                json!({ "cmd": ".moa/skills/db-backed-package/scripts/run.sh" }),
                "run_script",
            ),
        ]))
        .push_response(ScriptedResponse::text("Package materialized."))
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
