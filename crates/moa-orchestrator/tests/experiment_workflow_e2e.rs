//! End-to-end coverage for workflow experiment target execution through Restate.

#![cfg(feature = "integration")]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::wire::{
    ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest, ArtifactPublishResponse,
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse, WorkflowRunStatus, WorkflowStatusRequest,
};
use moa_core::{ActionRuleScope, TenantId, WorkspaceId};
use moa_test_support::postgres::test_database_url;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_tenant_admin,
    register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
    test_user_identity, with_identity,
};

mod support;

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
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
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for workflow experiment e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_experiment_links_queued_artifact_workflow_run() -> Result<()> {
    // Pins: workflow experiments start WorkflowRuntime runs and expose the linked queued run.
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
    let workspace_id = WorkspaceId::new(tenant_id.to_string());
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let published =
            import_and_publish_damaged_food_workflow(&client, ingress, &identity, &workspace_id)
                .await?;

        let run =
            run_workflow_experiment(&client, ingress, &identity, &workspace_id, &published).await?;
        assert_eq!(run.status, "accepted");
        assert_ne!(run.score_run_id, Uuid::nil());
        assert!(
            run.workflow_run_uid.is_none(),
            "workflow run should be linked by ExperimentRun after admission"
        );

        let experiment_status = wait_for_linked_workflow_experiment(
            &client,
            ingress,
            &identity,
            &workspace_id,
            run.run_uid,
        )
        .await?;
        let workflow_run_uid = experiment_status
            .workflow_run_uid
            .context("experiment status should expose linked workflow_run_uid")?;
        let workflow_status =
            workflow_status(&client, ingress, &identity, &workspace_id, workflow_run_uid).await?;

        assert_eq!(experiment_status.target_kind.as_deref(), Some("workflow"));
        assert_eq!(experiment_status.score_run_id, Some(run.score_run_id));
        assert_eq!(
            experiment_status.workflow_run_uid,
            Some(workflow_status.run_id)
        );
        assert_eq!(experiment_status.status, workflow_status.status);
        assert_eq!(workflow_status.status, "queued");
        assert!(workflow_status.current_node_id.is_none());
        assert!(
            workflow_status.node_runs.is_empty(),
            "workflow interpreter has not executed nodes yet"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn import_and_publish_damaged_food_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
) -> Result<ArtifactPublishResponse> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(
            Uuid::parse_str(workspace_id.as_str()).context("workspace id is tenant uuid")?,
        ),
    };
    let import_request = ArtifactImportRequest {
        scope,
        source_format: "yaml".to_string(),
        source_text: damaged_food_workflow_source().to_string(),
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

async fn run_workflow_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    published: &ArtifactPublishResponse,
) -> Result<ExperimentRunResponse> {
    let order_id = format!("ORD-{}", Uuid::now_v7());
    let request = ExperimentRunRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        name: "damaged-food-workflow-experiment".to_string(),
        plan_revision_uid: None,
        target: Some(json!({
            "kind": "workflow",
            "workflow_ref": "workflow://damaged-food-replacement",
            "input": {
                "order_id": order_id,
                "damage_summary": "clear photo shows sauce leaked through the delivery bag",
                "customer_requested": "refund_or_replacement"
            },
            "session_id": null,
            "idempotency_key": format!("workflow-target-{}", Uuid::now_v7())
        })),
        variant: Some(json!({
            "name": "damaged-food-workflow",
            "model": null,
            "artifact_revision_uids": [published.revision_uid],
            "skill_refs": [],
            "workflow_ref": "workflow://damaged-food-replacement",
            "metadata": { "lane": "workflow-experiment-e2e" }
        })),
        scorecard: json!({
            "score_names": ["workflow_started"],
            "evaluator_metadata": { "mode": "manual-or-later" }
        }),
        score_run_id: None,
        idempotency_key: Some(format!("experiment-workflow-{}", Uuid::now_v7())),
        agent_revision_variants: Vec::new(),
    };
    post_json_with_identity(client, ingress, "Experiments", "run", identity, &request)
        .await?
        .json::<ExperimentRunResponse>()
        .await
        .context("deserialize experiment run response")
}

async fn wait_for_linked_workflow_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_uid: Uuid,
) -> Result<ExperimentRunStatusResponse> {
    let request = ExperimentRunStatusRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        run_uid,
    };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Experiments", "status", identity, &request)
                .await?
                .json::<ExperimentRunStatusResponse>()
                .await
                .context("deserialize experiment status response")?;
        if status.status == "failed" {
            bail!("workflow experiment failed before linking a workflow run: {status:?}");
        }
        if status.workflow_run_uid.is_some() && status.status == "queued" {
            return Ok(status);
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for experiment {run_uid} to link a queued workflow run; last status: {last_status:?}"
    )
}

async fn workflow_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_id: Uuid,
) -> Result<WorkflowRunStatus> {
    let request = WorkflowStatusRequest {
        tenant_id: TenantId::from(
            Uuid::parse_str(workspace_id.as_str()).context("workspace id is tenant uuid")?,
        ),
        run_id,
    };
    post_json_with_identity(client, ingress, "Workflows", "status", identity, &request)
        .await?
        .json::<WorkflowRunStatus>()
        .await
        .context("deserialize workflow status response")
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

fn damaged_food_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: damaged-food-replacement
  description: Procedure for refund, credit, or replacement when food arrives damaged.
  tags:
    - support
    - food-delivery
    - refund
status: draft
definition:
  type: workflow
  spec:
    input_schema:
      type: object
      required:
        - order_id
        - damage_summary
      properties:
        order_id:
          type: string
        damage_summary:
          type: string
        customer_requested:
          type: string
    state_schema:
      type: object
      properties:
        evidence_sufficient:
          type: boolean
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: verify_evidence
        kind: condition
        condition:
          type: exists
          path: $.damage_summary
        ui:
          x: 280
          y: 120
      - id: choose_resolution
        kind: agent
        max_turns: 2
        input:
          instruction: Decide refund, credit, replacement, or escalation after evidence review.
        ui:
          x: 520
          y: 120
      - id: notify_customer
        kind: action
        input:
          template: Tell the customer the resolution and timing.
        ui:
          x: 760
          y: 120
      - id: done
        kind: end
        ui:
          x: 980
          y: 120
    edges:
      - id: start-to-verify
        from: start
        to: verify_evidence
      - id: verify-to-resolution
        from: verify_evidence
        to: choose_resolution
      - id: resolution-to-notify
        from: choose_resolution
        to: notify_customer
      - id: notify-to-done
        from: notify_customer
        to: done
"#
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

fn tenant_id_from_workspace(workspace_id: &WorkspaceId) -> Result<TenantId> {
    Uuid::parse_str(workspace_id.as_str())
        .map(TenantId::from)
        .context("workspace fixture id should be a tenant UUID")
}
