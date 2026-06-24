//! End-to-end coverage for artifact workflow execution through Restate.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::wire::{
    ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest, ArtifactPublishResponse,
    WorkflowCancelRequest, WorkflowCancelResponse, WorkflowReviewDecisionKind,
    WorkflowReviewDecisionRequest, WorkflowReviewDecisionResponse, WorkflowRunRequest,
    WorkflowRunResponse, WorkflowRunStatus, WorkflowSignalRequest, WorkflowSignalResponse,
    WorkflowStatusRequest,
};
use moa_core::{
    ActionRuleScope, Event, EventRange, EventRecord, ModelId, SessionId, SubAgentChildRef, TenantId,
};
use moa_test_support::postgres::test_database_url;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_admin, grant_tenant_operator, register_deployment, reserve_orchestrator_ports,
    restate_admin_url, restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, test_session_meta,
};

mod support;

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    provider_override_fixture: Option<&Path>,
) -> Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"));
    command
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env("RUST_LOG", "info")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = provider_override_fixture {
        command.env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", path.display()),
        );
    }
    command
        .spawn()
        .context("spawn moa-orchestrator binary for workflow execution e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_execution_runs_deterministic_nodes_service_e2e() -> Result<()> {
    // Pins: `Workflows::run` starts `ArtifactWorkflowExecution` and persists node projections.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            deterministic_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://deterministic-routing",
            json!({ "decision": "approved" }),
        )
        .await?;
        assert_eq!(run.status, "queued");

        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert_eq!(status.run_id, run.run_id);
        assert_eq!(status.status, "completed");
        assert_eq!(status.current_node_id.as_deref(), Some("approved"));
        assert_eq!(status.output, Some(json!({ "route": "approved" })));
        assert_eq!(node_ids(&status), vec!["start", "route", "approved"]);
        assert!(
            status
                .node_runs
                .iter()
                .all(|node_run| node_run.status == "completed" && node_run.completed_at.is_some()),
            "all deterministic node runs should be terminal: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_tool_node_executes_through_tool_executor_service_e2e() -> Result<()> {
    // Pins: tool workflow nodes execute through policy and ToolExecutor, then resume the graph.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            tool_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://tool-search-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "search", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["search"]["is_error"].as_bool()),
            Some(false)
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_review_node_pauses_and_resumes_service_e2e() -> Result<()> {
    // Pins: review nodes pause as pending_review and resume through Workflows/decide_review.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            review_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://review-gated-workflow",
            json!({}),
        )
        .await?;
        let pending = wait_for_workflow_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));
        assert_eq!(node_ids(&pending), vec!["start", "gate"]);
        assert_eq!(pending.node_runs[1].status, "pending_review");

        let decision =
            decide_workflow_review(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(decision.accepted);
        assert_eq!(decision.status, "pending_review");

        let completed =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&completed), vec!["start", "gate", "done"]);

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_wait_signal_node_pauses_and_resumes_service_e2e() -> Result<()> {
    // Pins: wait_signal nodes keep the workflow body alive until Workflows/signal resolves it.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_operator(&identity, tenant_id).await?;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            wait_signal_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://signal-gated-workflow",
            json!({}),
        )
        .await?;
        let waiting = wait_for_workflow_status(
            &client, ingress, &identity, tenant_id, run.run_id, "running",
        )
        .await?;
        assert_eq!(waiting.current_node_id.as_deref(), Some("signal"));
        assert_eq!(node_ids(&waiting), vec!["start", "signal"]);

        let signal = signal_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(signal.accepted);
        assert_eq!(signal.status, "running");

        let completed =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert_eq!(completed.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&completed), vec!["start", "signal", "done"]);
        assert_eq!(
            completed
                .output
                .as_ref()
                .and_then(|value| value["signal"]["payload"]["ticket"].as_str()),
            Some("T-123")
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_cancel_resolves_paused_review_service_e2e() -> Result<()> {
    // Pins: Workflows/cancel resolves the artifact workflow cancel promise while blocked.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            review_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://review-gated-workflow",
            json!({}),
        )
        .await?;
        let pending = wait_for_workflow_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));

        let cancelled = cancel_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(cancelled.cancelled);

        let status = wait_for_workflow_node_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            "cancelled",
            "gate",
            "cancelled",
        )
        .await?;
        assert_eq!(status.current_node_id.as_deref(), Some("gate"));
        assert_eq!(node_ids(&status), vec!["start", "gate"]);
        assert_eq!(status.node_runs[1].status, "cancelled");

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn workflow_agent_node_uses_session_turn_service_e2e() -> Result<()> {
    // Pins: workflow Agent nodes are deterministic-skill adapters into Session/TurnExecution.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("workflow-agent-script.json");
    let final_text = "The workflow agent turn completed through the existing session loop.";
    write_scripted_agent_fixture(&fixture_path, final_text)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut meta = test_session_meta(&format!("workflow-agent-{}", Uuid::now_v7()));
    meta.model = ModelId::new("scripted-loadtest");
    let tenant_id = meta.tenant_id;
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            agent_workflow_source(),
        )
        .await?;

        let run = start_workflow_with_session(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://agent-adapter-workflow",
            json!({}),
            Some(session_id),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "agent", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["agent"]["message"].as_str()),
            Some(final_text)
        );

        let events = fetch_events(&client, ingress, &identity, session_id).await?;
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                Event::UserMessage { text, .. }
                    if text == "Summarize the deterministic skill adapter status."
            )),
            "workflow agent node should persist the user message in the session event log"
        );
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text == final_text
            )),
            "workflow agent node should persist the brain response in the session event log"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_sub_agent_node_enforces_fanout_limits_service_e2e() -> Result<()> {
    // Pins: workflow SubAgent nodes reuse the existing root-session delegation fan-out limit.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let meta = test_session_meta(&format!("workflow-sub-agent-{}", Uuid::now_v7()));
    let tenant_id = meta.tenant_id;
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;
        seed_active_session_children(&client, ingress, session_id).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            sub_agent_workflow_source(),
        )
        .await?;

        let run = start_workflow_with_session(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://sub-agent-fanout-workflow",
            json!({}),
            Some(session_id),
        )
        .await?;
        let status =
            wait_for_workflow_status(&client, ingress, &identity, tenant_id, run.run_id, "failed")
                .await?;

        assert_eq!(status.current_node_id.as_deref(), Some("delegate"));
        assert_eq!(node_ids(&status), vec!["start", "delegate"]);
        assert_eq!(status.node_runs[1].status, "failed");
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("fan-out limit")),
            "workflow sub-agent node should fail through delegation fan-out validation: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_parallel_join_executes_independent_tool_nodes_service_e2e() -> Result<()> {
    // Pins: workflow Parallel/Join topology persists visible branch node runs.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            parallel_join_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://parallel-join-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(
            node_ids(&status),
            vec!["start", "fanout", "left", "right", "join", "done"]
        );
        assert!(
            status
                .node_runs
                .iter()
                .all(|node_run| node_run.status == "completed"),
            "parallel workflow should complete every visible node: {status:?}"
        );
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["left"]["is_error"].as_bool()),
            Some(false)
        );
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["right"]["is_error"].as_bool()),
            Some(false)
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_loop_guard_stops_infinite_refinement_service_e2e() -> Result<()> {
    // Pins: workflow loop guards fail explicit back-edges instead of spinning indefinitely.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            loop_guard_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://loop-guard-workflow",
            json!({ "retry": true }),
        )
        .await?;
        let status =
            wait_for_workflow_status(&client, ingress, &identity, tenant_id, run.run_id, "failed")
                .await?;

        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("exceeded max iterations")),
            "loop guard should fail the workflow with a typed error: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, AGE, and pgvector"]
async fn workflow_memory_read_respects_contact_scope_service_e2e() -> Result<()> {
    // Pins: workflow MemoryRead nodes execute through the scoped Memory service adapter.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    grant_tenant_operator(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            memory_read_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://memory-read-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "recall", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["recall"]["contact_id"].as_str()),
            Some("22222222-2222-2222-2222-222222222222")
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, AGE, and pgvector"]
async fn workflow_memory_write_records_scoped_fact_service_e2e() -> Result<()> {
    // Pins: workflow MemoryWrite nodes execute through graph-memory ingestion with provenance.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    grant_tenant_operator(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            memory_write_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://memory-write-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "remember", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["remember"]["contact_id"].as_str()),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert!(
            status
                .output
                .as_ref()
                .and_then(|value| value["remember"]["results"].as_array())
                .is_some_and(|results| results.len() == 1),
            "memory write should report one ingested document: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn import_and_publish_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    source_text: &str,
) -> Result<ArtifactPublishResponse> {
    let scope = ActionRuleScope::Tenant { tenant_id };
    let import_request = ArtifactImportRequest {
        scope,
        source_format: "yaml".to_string(),
        source_text: source_text.to_string(),
        files: Vec::new(),
    };
    let imported = post_json_with_identity(
        client,
        ingress,
        "Artifacts",
        "import",
        identity,
        &import_request,
    )
    .await?
    .json::<ArtifactImportResponse>()
    .await
    .context("deserialize artifact import response")?;
    assert_eq!(imported.status, "draft");

    let publish_request = ArtifactPublishRequest {
        scope,
        revision_uid: imported.revision_uid,
    };
    let published = post_json_with_identity(
        client,
        ingress,
        "Artifacts",
        "publish",
        identity,
        &publish_request,
    )
    .await?
    .json::<ArtifactPublishResponse>()
    .await
    .context("deserialize artifact publish response")?;
    assert_eq!(published.status, "published");
    assert_validation_report_has_no_errors(&published.validation_report)?;
    Ok(published)
}

async fn start_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    workflow_ref: &str,
    input: Value,
) -> Result<WorkflowRunResponse> {
    start_workflow_with_session(
        client,
        ingress,
        identity,
        tenant_id,
        workflow_ref,
        input,
        None,
    )
    .await
}

async fn start_workflow_with_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    workflow_ref: &str,
    input: Value,
    session_id: Option<SessionId>,
) -> Result<WorkflowRunResponse> {
    let request = WorkflowRunRequest {
        tenant_id,
        workflow_ref: workflow_ref.to_string(),
        input,
        session_id,
        idempotency_key: Some(format!("workflow-{}", Uuid::now_v7())),
    };
    post_json_with_identity(client, ingress, "Workflows", "run", identity, &request)
        .await?
        .json::<WorkflowRunResponse>()
        .await
        .context("deserialize workflow run response")
}

async fn create_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    meta: &moa_core::SessionMeta,
) -> Result<SessionId> {
    let create_request = client.post(service_url(ingress, "SessionStore", "create_session"));
    let session_id = with_identity(create_request, identity)
        .json(meta)
        .send()
        .await
        .context("create session via Restate ingress")?
        .error_for_status()
        .context("create_session should succeed")?
        .json::<SessionId>()
        .await
        .context("deserialize create_session response")?;
    grant_session_participant(identity, session_id).await?;

    client
        .post(service_url(ingress, "SessionStore", "init_session_vo"))
        .json(&init_session_vo_request(session_id, meta.clone()))
        .send()
        .await
        .context("initialize session VO state")?
        .error_for_status()
        .context("init_session_vo should succeed")?;

    Ok(session_id)
}

async fn seed_active_session_children(
    client: &reqwest::Client,
    ingress: &str,
    session_id: SessionId,
) -> Result<()> {
    for index in 0..4 {
        let child = SubAgentChildRef {
            id: format!("{session_id}-active-child-{index}"),
            task_hash: format!("active-hash-{index}"),
            budget_tokens: 256,
            terminal: None,
        };
        client
            .post(object_url(ingress, "Session", session_id, "register_child"))
            .json(&child)
            .send()
            .await
            .context("seed active session child")?
            .error_for_status()
            .context("register_child should accept seeded active child")?;
    }
    Ok(())
}

async fn fetch_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    post_json_with_identity(
        client,
        ingress,
        "SessionStore",
        "get_events",
        identity,
        &get_events_request(session_id, EventRange::all()),
    )
    .await?
    .json::<Vec<EventRecord>>()
    .await
    .context("deserialize session events")
}

async fn decide_workflow_review(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowReviewDecisionResponse> {
    let request = WorkflowReviewDecisionRequest {
        tenant_id,
        run_id,
        node_id: Some("gate".to_string()),
        decision: WorkflowReviewDecisionKind::Approved,
        reason: Some("approved in workflow review e2e".to_string()),
        output: Some(json!({ "approved": true })),
    };
    post_json_with_identity(
        client,
        ingress,
        "Workflows",
        "decide_review",
        identity,
        &request,
    )
    .await?
    .json::<WorkflowReviewDecisionResponse>()
    .await
    .context("deserialize workflow review decision response")
}

async fn signal_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowSignalResponse> {
    let request = WorkflowSignalRequest {
        tenant_id,
        run_id,
        node_id: Some("signal".to_string()),
        signal_name: Some("ticket_ready".to_string()),
        payload: json!({ "ticket": "T-123" }),
    };
    post_json_with_identity(client, ingress, "Workflows", "signal", identity, &request)
        .await?
        .json::<WorkflowSignalResponse>()
        .await
        .context("deserialize workflow signal response")
}

async fn cancel_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowCancelResponse> {
    let request = WorkflowCancelRequest {
        tenant_id,
        run_id,
        reason: Some("cancelled in workflow e2e".to_string()),
    };
    post_json_with_identity(client, ingress, "Workflows", "cancel", identity, &request)
        .await?
        .json::<WorkflowCancelResponse>()
        .await
        .context("deserialize workflow cancel response")
}

async fn wait_for_completed_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowRunStatus> {
    wait_for_workflow_status(client, ingress, identity, tenant_id, run_id, "completed").await
}

async fn wait_for_workflow_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
    expected: &str,
) -> Result<WorkflowRunStatus> {
    let request = WorkflowStatusRequest { tenant_id, run_id };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Workflows", "status", identity, &request)
                .await?
                .json::<WorkflowRunStatus>()
                .await
                .context("deserialize workflow status response")?;
        if status.status == expected {
            return Ok(status);
        }
        if status.status == "failed" {
            bail!("workflow run failed before reaching {expected}: {status:?}");
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for workflow run {run_id} to reach {expected}; last status: {last_status:?}"
    )
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_workflow_node_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
    expected_run_status: &str,
    node_id: &str,
    expected_node_status: &str,
) -> Result<WorkflowRunStatus> {
    let request = WorkflowStatusRequest { tenant_id, run_id };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Workflows", "status", identity, &request)
                .await?
                .json::<WorkflowRunStatus>()
                .await
                .context("deserialize workflow status response")?;
        let node_matches = status
            .node_runs
            .iter()
            .any(|node_run| node_run.node_id == node_id && node_run.status == expected_node_status);
        if status.status == expected_run_status && node_matches {
            return Ok(status);
        }
        if status.status == "failed" {
            bail!("workflow run failed before reaching {expected_run_status}: {status:?}");
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for workflow run {run_id} to reach {expected_run_status} with {node_id}={expected_node_status}; last status: {last_status:?}"
    )
}

async fn post_json_with_identity<T: serde::Serialize + ?Sized>(
    client: &reqwest::Client,
    ingress: &str,
    service: &str,
    handler: &str,
    identity: &Identity,
    request: &T,
) -> Result<reqwest::Response> {
    let response = with_identity(
        client.post(service_url(ingress, service, handler)),
        identity,
    )
    .json(request)
    .send()
    .await
    .with_context(|| format!("call {service}/{handler}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    bail!("{service}/{handler} returned {status}: {body}")
}

fn service_url(ingress: &str, service: &str, handler: &str) -> String {
    format!("{}/{service}/{handler}", ingress.trim_end_matches('/'))
}

fn object_url(ingress: &str, service: &str, object_id: SessionId, handler: &str) -> String {
    format!(
        "{}/{service}/{object_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn node_ids(status: &WorkflowRunStatus) -> Vec<&str> {
    status
        .node_runs
        .iter()
        .map(|node_run| node_run.node_id.as_str())
        .collect()
}

fn deterministic_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: deterministic-routing
  description: Deterministic branch workflow for execution projection tests.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    input_schema:
      type: object
      properties:
        decision:
          type: string
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: route
        kind: condition
        ui:
          x: 280
          y: 120
      - id: approved
        kind: end
        input:
          route: approved
        ui:
          x: 520
          y: 80
      - id: rejected
        kind: end
        input:
          route: rejected
        ui:
          x: 520
          y: 160
    edges:
      - id: start-route
        from: start
        to: route
      - id: route-approved
        from: route
        to: approved
        when:
          type: equals
          left: input.decision
          right: approved
      - id: route-rejected
        from: route
        to: rejected
    ui:
      layout: dagre
"#
}

fn tool_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: tool-search-workflow
  description: Workflow that executes one idempotent tool node.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: search
        kind: tool
        tool_refs:
          - tool://file_search
        input:
          pattern: "*"
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-search
        from: start
        to: search
      - id: search-done
        from: search
        to: done
    ui:
      layout: dagre
"#
}

fn review_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: review-gated-workflow
  description: Workflow that pauses on an explicit review node.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: gate
        kind: review
        input:
          prompt: Approve before completing the workflow.
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        input:
          reviewed: true
        ui:
          x: 520
          y: 120
    edges:
      - id: start-gate
        from: start
        to: gate
      - id: gate-done
        from: gate
        to: done
    ui:
      layout: dagre
"#
}

fn wait_signal_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: signal-gated-workflow
  description: Workflow that pauses until an external signal arrives.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: signal
        kind: wait_signal
        input:
          name: ticket_ready
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-signal
        from: start
        to: signal
      - id: signal-done
        from: signal
        to: done
    ui:
      layout: dagre
"#
}

fn agent_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: agent-adapter-workflow
  description: Workflow that adapts one deterministic graph node into a session turn.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: agent
        kind: agent
        max_turns: 1
        input:
          prompt: Summarize the deterministic skill adapter status.
          model: scripted-loadtest
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-agent
        from: start
        to: agent
      - id: agent-done
        from: agent
        to: done
    ui:
      layout: dagre
"#
}

fn sub_agent_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: sub-agent-fanout-workflow
  description: Workflow that adapts one deterministic graph node into sub-agent delegation.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: delegate
        kind: sub_agent
        max_turns: 1
        input:
          task: Inspect whether this workflow node respects existing delegation fan-out limits.
          task_name: fanout-check
          tool_subset: []
          budget_tokens: 256
          timeout_ms: 0
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-delegate
        from: start
        to: delegate
      - id: delegate-done
        from: delegate
        to: done
    ui:
      layout: dagre
"#
}

fn parallel_join_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: parallel-join-workflow
  description: Workflow that fans out two independent tool nodes and joins them.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 160
      - id: fanout
        kind: parallel
        ui:
          x: 240
          y: 160
      - id: left
        kind: tool
        tool_refs:
          - tool://file_search
        input:
          pattern: "*"
        ui:
          x: 420
          y: 80
      - id: right
        kind: tool
        tool_refs:
          - tool://file_search
        input:
          pattern: "*"
        ui:
          x: 420
          y: 240
      - id: join
        kind: join
        ui:
          x: 620
          y: 160
      - id: done
        kind: end
        ui:
          x: 780
          y: 160
    edges:
      - id: start-fanout
        from: start
        to: fanout
      - id: fanout-left
        from: fanout
        to: left
      - id: fanout-right
        from: fanout
        to: right
      - id: left-join
        from: left
        to: join
      - id: right-join
        from: right
        to: join
      - id: join-done
        from: join
        to: done
    ui:
      layout: dagre
"#
}

fn loop_guard_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: loop-guard-workflow
  description: Workflow with an explicit conditional back-edge for loop guard coverage.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: retry
        kind: condition
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-retry
        from: start
        to: retry
      - id: retry-loop
        from: retry
        to: retry
        when:
          type: exists
          path: input.retry
      - id: retry-done
        from: retry
        to: done
    ui:
      layout: dagre
"#
}

fn memory_read_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: memory-read-workflow
  description: Workflow that reads contact-scoped graph memory through the Memory service.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: recall
        kind: memory_read
        input:
          contact_id: "22222222-2222-2222-2222-222222222222"
          query: "preferred support channel"
          limit: 5
          label_filter:
            - Fact
          max_pii_class: restricted
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-recall
        from: start
        to: recall
      - id: recall-done
        from: recall
        to: done
    ui:
      layout: dagre
"#
}

fn memory_write_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: memory-write-workflow
  description: Workflow that writes a scoped graph-memory fact through ingestion.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: remember
        kind: memory_write
        input:
          contact_id: "22222222-2222-2222-2222-222222222222"
          source_name: workflow memory note
          content: "The customer prefers email updates for support tickets."
          metadata:
            source: workflow-e2e
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-remember
        from: start
        to: remember
      - id: remember-done
        from: remember
        to: done
    ui:
      layout: dagre
"#
}

fn write_scripted_agent_fixture(path: &Path, final_text: &str) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": final_text,
                "tool_calls": []
            }
        }
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted agent fixture")?;
    fs::write(path, body).context("write scripted agent fixture")
}

fn assert_validation_report_has_no_errors(report: &Value) -> Result<()> {
    let Some(errors) = report.get("errors").and_then(Value::as_array) else {
        bail!("validation report did not include an errors array: {report}");
    };
    if errors.is_empty() {
        return Ok(());
    }

    bail!("published workflow had validation errors: {errors:?}")
}
