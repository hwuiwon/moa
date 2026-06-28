use chrono::TimeDelta;
use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactFile, ArtifactNodeRunStatus, ArtifactNodeRunUpdate, ArtifactRegistry,
    ArtifactRunStatus, ArtifactRunUpdate, MAX_FILE_SIZE_BYTES, NewArtifactDraft, NewArtifactFile,
    NewArtifactNodeRun, NewArtifactRun,
};
use moa_artifacts::validation::validate_for_status;
use moa_core::{ActionRuleScope, MoaError, Result, SessionId, TenantId};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_missing_id_lookups_return_none_db_memory() -> Result<()> {
    // Pins: revision/run/published lookups for unknown ids are Ok(None), not errors.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };

    assert!(
        registry
            .load_revision(&scope, Uuid::now_v7())
            .await?
            .is_none(),
        "unknown revision uid should resolve to None"
    );
    assert!(
        registry.load_run(&scope, Uuid::now_v7()).await?.is_none(),
        "unknown run uid should resolve to None"
    );
    assert!(
        registry
            .load_visible_published(&scope, ArtifactKind::Skill, "does-not-exist")
            .await?
            .is_none(),
        "unknown published artifact should resolve to None"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_round_trips_file_content_type_and_empty_body_db_memory() -> Result<()> {
    // Pins: explicit content_type and executable flags round-trip, and an empty file body persists as zero bytes.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("file-roundtrip-{}", Uuid::now_v7());
    let document = skill_doc(&name, "file round trip");
    let source = document.to_yaml().expect("serialize doc");

    let files = vec![
        NewArtifactFile {
            path: "SKILL.md".to_string(),
            content: b"# Skill\n".to_vec(),
            content_type: Some("text/markdown".to_string()),
            executable: false,
        },
        NewArtifactFile {
            path: "run.sh".to_string(),
            content: b"#!/bin/sh\necho hi\n".to_vec(),
            content_type: Some("application/x-sh".to_string()),
            executable: true,
        },
        NewArtifactFile {
            path: "EMPTY".to_string(),
            content: Vec::new(),
            content_type: Some("application/octet-stream".to_string()),
            executable: false,
        },
    ];

    let draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &files,
            },
        )
        .await?;

    let loaded = registry.load_files(&scope, draft.revision_uid).await?;

    let markdown = file_by_path(&loaded, "SKILL.md");
    assert_eq!(markdown.content_type.as_deref(), Some("text/markdown"));
    assert_eq!(markdown.content, b"# Skill\n");
    assert!(!markdown.executable);

    let script = file_by_path(&loaded, "run.sh");
    assert_eq!(script.content_type.as_deref(), Some("application/x-sh"));
    assert!(script.executable);

    let empty = file_by_path(&loaded, "EMPTY");
    assert_eq!(
        empty.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert!(empty.content.is_empty());
    assert_eq!(empty.file_size_bytes, 0);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_rejects_oversize_artifact_file_db_memory() -> Result<()> {
    // Pins: a file larger than MAX_FILE_SIZE_BYTES is rejected with a ValidationError and leaves nothing persisted.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("oversize-{}", Uuid::now_v7());
    let document = skill_doc(&name, "oversize file");
    let source = document.to_yaml().expect("serialize doc");
    let oversize = vec![0u8; MAX_FILE_SIZE_BYTES + 1];

    let error = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile::new("BIG.bin", oversize)],
            },
        )
        .await
        .expect_err("oversize artifact file must reject");
    assert!(
        matches!(error, MoaError::ValidationError(_)),
        "expected ValidationError, got {error:?}"
    );
    assert!(
        error.to_string().contains("too large"),
        "unexpected message: {error}"
    );

    // The failed draft transaction rolled back: nothing for this name is visible.
    assert!(
        registry
            .load_visible(&scope, ArtifactKind::Skill, &name)
            .await?
            .is_none(),
        "rolled-back oversize draft must not persist an artifact"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn file_by_path<'a>(files: &'a [ArtifactFile], path: &str) -> &'a ArtifactFile {
    files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("expected stored file at {path}, got {files:?}"))
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_preserves_tenant_published_revision_history() -> Result<()> {
    // Pins: artifact visibility and revision history are tenant-scoped.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let name = format!("artifact-scope-{}", Uuid::now_v7());
    let tenant_scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };

    let tenant_doc = skill_doc(&name, "tenant-v1");
    let tenant_source = tenant_doc.to_yaml().expect("serialize tenant doc");
    let tenant_v1 = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &tenant_doc,
                source_format: "yaml",
                source_text: tenant_source.as_bytes(),
                files: &[NewArtifactFile::new(
                    "SKILL.md",
                    b"# Tenant skill\n".to_vec(),
                )],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            tenant_v1.revision_uid,
            &validate_for_status(&tenant_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_tenant = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant artifact visible");
    assert_eq!(visible_tenant.scope, "tenant");
    assert_eq!(visible_tenant.version, 1);

    let tenant_v2_doc = skill_doc(&name, "tenant-v2");
    let tenant_v2_source = tenant_v2_doc.to_yaml().expect("serialize tenant v2 doc");
    let tenant_v2 = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &tenant_v2_doc,
                source_format: "yaml",
                source_text: tenant_v2_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            tenant_v2.revision_uid,
            &validate_for_status(&tenant_v2_doc, ArtifactStatus::Published),
        )
        .await?;
    let visible_tenant_v2 = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant artifact v2 visible");
    assert_eq!(visible_tenant_v2.version, 2);
    assert_eq!(visible_tenant_v2.description, "tenant-v2");
    let loaded_tenant_v1 = registry
        .load_revision(&tenant_scope, tenant_v1.revision_uid)
        .await?
        .expect("tenant v1 remains loadable by exact revision id");
    assert_eq!(loaded_tenant_v1.version, 1);
    assert_eq!(loaded_tenant_v1.status, ArtifactStatus::Published);
    assert_eq!(loaded_tenant_v1.valid_to, None);

    let summaries = registry
        .list_visible(
            &tenant_scope,
            Some(ArtifactKind::Skill),
            Some(ArtifactStatus::Published),
        )
        .await?;
    let matching = summaries
        .iter()
        .filter(|summary| summary.name == name)
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].scope, "tenant");

    let files = registry
        .load_files(&tenant_scope, tenant_v1.revision_uid)
        .await?;
    assert_eq!(files[0].path, "SKILL.md");

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_persists_behavior_lab_artifact_kinds() -> Result<()> {
    // Pins: the DB registry accepts behavior-lab artifact kinds through the forward constraint.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let workspace_scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("checkout-plan-{}", Uuid::now_v7());
    let document = experiment_plan_doc(&name);
    let source = document.to_json().expect("serialize behavior-lab doc");

    let draft = registry
        .create_draft(
            &workspace_scope,
            NewArtifactDraft {
                document: &document,
                source_format: "json",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    let published = registry
        .publish_revision(
            &workspace_scope,
            draft.revision_uid,
            &validate_for_status(&document, ArtifactStatus::Published),
        )
        .await?;

    assert_eq!(published.kind, ArtifactKind::ExperimentPlan);
    assert_eq!(published.status, ArtifactStatus::Published);

    let loaded = registry
        .load_visible_published(&workspace_scope, ArtifactKind::ExperimentPlan, &name)
        .await?
        .expect("published experiment plan is visible");
    assert_eq!(loaded.revision_uid, published.revision_uid);
    assert_eq!(loaded.source_format, "json");
    assert_eq!(loaded.document.kind, ArtifactKind::ExperimentPlan);

    let summaries = registry
        .list_visible(
            &workspace_scope,
            Some(ArtifactKind::ExperimentPlan),
            Some(ArtifactStatus::Published),
        )
        .await?;
    assert_eq!(
        summaries
            .iter()
            .filter(|summary| summary.name == name)
            .count(),
        1
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn workflow_run_node_projection_db_memory() -> Result<()> {
    // Pins: workflow runs and node-run projections round-trip through the registry schema.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let workspace_scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("support-flow-{}", Uuid::now_v7());
    let document = workflow_doc(&name);
    let source = document.to_json().expect("serialize workflow doc");
    let input = json!({ "ticket_id": "T-100", "priority": "high" });
    let initial_state = json!({ "steps": [] });
    let session_id = SessionId::new();
    let idempotency_key = Some(format!("workflow-run-{}", Uuid::now_v7()));

    let draft = registry
        .create_draft(
            &workspace_scope,
            NewArtifactDraft {
                document: &document,
                source_format: "json",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    let published = registry
        .publish_revision(
            &workspace_scope,
            draft.revision_uid,
            &validate_for_status(&document, ArtifactStatus::Published),
        )
        .await?;

    let appended = registry
        .append_run(
            &workspace_scope,
            NewArtifactRun {
                artifact_uid: Some(published.artifact_uid),
                revision_uid: Some(published.revision_uid),
                session_id: Some(session_id),
                workflow_ref: format!("workflow://{name}"),
                status: ArtifactRunStatus::Queued,
                current_node_id: None,
                input: input.clone(),
                state: initial_state,
                output: None,
                error: None,
                idempotency_key: idempotency_key.clone(),
            },
        )
        .await?;
    let duplicate = registry
        .append_run(
            &workspace_scope,
            NewArtifactRun {
                artifact_uid: Some(published.artifact_uid),
                revision_uid: Some(published.revision_uid),
                session_id: Some(session_id),
                workflow_ref: format!("workflow://{name}"),
                status: ArtifactRunStatus::Queued,
                current_node_id: None,
                input: json!({ "ticket_id": "T-999" }),
                state: json!({ "steps": ["duplicate"] }),
                output: None,
                error: None,
                idempotency_key,
            },
        )
        .await?;

    assert_eq!(appended.artifact_uid, Some(published.artifact_uid));
    assert_eq!(appended.revision_uid, Some(published.revision_uid));
    assert_eq!(appended.workflow_ref, format!("workflow://{name}"));
    assert_eq!(appended.input, input);
    assert_eq!(appended.state, json!({ "steps": [] }));
    assert_eq!(duplicate.run_uid, appended.run_uid);
    assert_eq!(duplicate.input, input);
    assert_eq!(duplicate.state, json!({ "steps": [] }));

    let loaded = registry
        .load_run(&workspace_scope, appended.run_uid)
        .await?
        .expect("workflow run should be visible after append");
    assert_eq!(loaded.artifact_uid, Some(published.artifact_uid));
    assert_eq!(loaded.revision_uid, Some(published.revision_uid));
    assert_eq!(loaded.session_id, Some(session_id));
    assert_eq!(loaded.workflow_ref, format!("workflow://{name}"));
    assert_eq!(loaded.status, ArtifactRunStatus::Queued);
    assert_eq!(loaded.input, input);
    assert_eq!(loaded.state, json!({ "steps": [] }));
    assert_eq!(loaded.output, None);

    let running_state = json!({ "steps": ["start"], "attempt": 1 });
    let running = registry
        .update_run(
            &workspace_scope,
            appended.run_uid,
            ArtifactRunUpdate {
                status: Some(ArtifactRunStatus::Running),
                current_node_id: Some(Some("start".to_string())),
                state: Some(running_state.clone()),
                output: None,
                error: None,
                completed_at: None,
            },
        )
        .await?
        .expect("running workflow run should update");
    assert_eq!(running.status, ArtifactRunStatus::Running);
    assert_eq!(running.current_node_id.as_deref(), Some("start"));
    assert_eq!(running.state, running_state);
    assert_eq!(running.completed_at, None);

    let node_run_uid = registry
        .append_node_run(
            &workspace_scope,
            NewArtifactNodeRun {
                run_uid: appended.run_uid,
                node_id: "start".to_string(),
                status: ArtifactNodeRunStatus::Running,
                input: json!({ "ticket_id": "T-100" }),
                output: None,
                error: None,
                completed_at: None,
            },
        )
        .await?;
    let duplicate_node_run_uid = registry
        .append_node_run(
            &workspace_scope,
            NewArtifactNodeRun {
                run_uid: appended.run_uid,
                node_id: "start".to_string(),
                status: ArtifactNodeRunStatus::Queued,
                input: json!({ "ticket_id": "T-duplicate" }),
                output: None,
                error: None,
                completed_at: None,
            },
        )
        .await?;
    assert_eq!(duplicate_node_run_uid, node_run_uid);

    let node_completed_at = running.started_at + TimeDelta::seconds(1);
    let completed_node = registry
        .update_node_run(
            &workspace_scope,
            node_run_uid,
            ArtifactNodeRunUpdate {
                status: Some(ArtifactNodeRunStatus::Completed),
                output: Some(Some(json!({ "accepted": true }))),
                error: Some(None),
                completed_at: Some(Some(node_completed_at)),
            },
        )
        .await?
        .expect("node run should update");
    assert_eq!(completed_node.status, ArtifactNodeRunStatus::Completed);
    assert_eq!(completed_node.output, Some(json!({ "accepted": true })));
    assert_eq!(completed_node.completed_at, Some(node_completed_at));

    let appended_node_uids = registry
        .append_node_runs(
            &workspace_scope,
            vec![
                NewArtifactNodeRun {
                    run_uid: appended.run_uid,
                    node_id: "start".to_string(),
                    status: ArtifactNodeRunStatus::Queued,
                    input: json!({ "ticket_id": "T-duplicate" }),
                    output: None,
                    error: None,
                    completed_at: None,
                },
                NewArtifactNodeRun {
                    run_uid: appended.run_uid,
                    node_id: "done".to_string(),
                    status: ArtifactNodeRunStatus::Queued,
                    input: json!({ "accepted": true }),
                    output: None,
                    error: None,
                    completed_at: None,
                },
            ],
        )
        .await?;
    assert_eq!(appended_node_uids[0], node_run_uid);
    let done_node_run_uid = appended_node_uids[1];

    let node_runs = registry
        .list_node_runs(&workspace_scope, appended.run_uid)
        .await?;
    assert_eq!(node_runs.len(), 2);
    assert_eq!(node_runs[0].node_run_uid, node_run_uid);
    assert_eq!(node_runs[0].node_id, "start");
    assert_eq!(node_runs[0].status, ArtifactNodeRunStatus::Completed);
    assert_eq!(node_runs[1].node_run_uid, done_node_run_uid);
    assert_eq!(node_runs[1].node_id, "done");
    assert_eq!(node_runs[1].status, ArtifactNodeRunStatus::Queued);
    assert!(
        node_runs[0].started_at <= node_runs[1].started_at,
        "node runs should be listed by started_at ASC"
    );

    let run_completed_at = running.started_at + TimeDelta::seconds(2);
    let completed_output = json!({ "resolution": "accepted" });
    let completed = registry
        .update_run(
            &workspace_scope,
            appended.run_uid,
            ArtifactRunUpdate {
                status: Some(ArtifactRunStatus::Completed),
                current_node_id: Some(Some("done".to_string())),
                state: Some(json!({ "steps": ["start", "done"], "attempt": 1 })),
                output: Some(Some(completed_output.clone())),
                error: Some(None),
                completed_at: Some(Some(run_completed_at)),
            },
        )
        .await?
        .expect("completed workflow run should update");
    assert_eq!(completed.status, ArtifactRunStatus::Completed);
    assert_eq!(completed.current_node_id.as_deref(), Some("done"));
    assert_eq!(completed.output, Some(completed_output.clone()));
    assert_eq!(completed.error, None);
    assert_eq!(completed.completed_at, Some(run_completed_at));

    let reloaded = registry
        .load_run(&workspace_scope, appended.run_uid)
        .await?
        .expect("completed workflow run should remain visible");
    assert_eq!(reloaded.artifact_uid, Some(published.artifact_uid));
    assert_eq!(reloaded.revision_uid, Some(published.revision_uid));
    assert_eq!(reloaded.workflow_ref, format!("workflow://{name}"));
    assert_eq!(reloaded.input, input);
    assert_eq!(
        reloaded.state,
        json!({ "steps": ["start", "done"], "attempt": 1 })
    );
    assert_eq!(reloaded.status, ArtifactRunStatus::Completed);
    assert_eq!(reloaded.current_node_id.as_deref(), Some("done"));
    assert_eq!(reloaded.output, Some(completed_output));
    assert_eq!(reloaded.completed_at, Some(run_completed_at));

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn skill_doc(name: &str, description: &str) -> ArtifactDocument {
    let source = json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": name,
            "description": description,
            "tags": ["test"]
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "inputs": { "type": "object" },
                "outputs": { "type": "object" }
            }
        }
    });
    serde_json::from_value(source).expect("test skill artifact is valid")
}

fn workflow_doc(name: &str) -> ArtifactDocument {
    let source = json!({
        "api_version": "moa.artifact/v1",
        "kind": "workflow",
        "metadata": {
            "name": name,
            "description": "Support workflow projection test",
            "tags": ["workflow"]
        },
        "definition": {
            "type": "workflow",
            "spec": {
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "ticket_id": { "type": "string" }
                    }
                },
                "state_schema": {
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    }
                },
                "nodes": [
                    { "id": "start", "kind": "start" },
                    { "id": "done", "kind": "end" }
                ],
                "edges": [
                    { "from": "start", "to": "done" }
                ]
            }
        }
    });
    serde_json::from_value(source).expect("test workflow artifact is valid")
}

fn experiment_plan_doc(name: &str) -> ArtifactDocument {
    let source = json!({
        "api_version": "moa.artifact/v1",
        "kind": "experiment_plan",
        "metadata": {
            "name": name,
            "description": "Checkout delay behavior-lab plan",
            "tags": ["behavior-lab"]
        },
        "definition": {
            "type": "experiment_plan",
            "spec": {
                "simulation": {
                    "scenarios": [{
                        "id": "checkout-delay",
                        "initial_situation": "The user asks why checkout is delayed.",
                        "goals": ["Understand the delay and next step."],
                        "success_criteria": ["The target gives a concrete next step."],
                        "failure_criteria": ["The target invents order facts."],
                        "max_turns": 8
                    }],
                    "personas": [{
                        "id": "careful-shopper",
                        "voice": "Patient and precise.",
                        "goals": ["Resolve the delay."],
                        "stop_behavior": "Stop after a concrete next step."
                    }],
                    "profiles": [{
                        "id": "vip-customer",
                        "facts": { "account_tier": "vip" }
                    }]
                },
                "target_variants": [{ "key": "agent-loop", "kind": "agent_loop" }],
                "simulator_model": "gpt-4.1-mini",
                "parallelism": 1,
                "trials_per_combination": 1,
                "budget": { "max_total_cents": 1000 }
            }
        }
    });
    serde_json::from_value(source).expect("test experiment plan artifact is valid")
}
