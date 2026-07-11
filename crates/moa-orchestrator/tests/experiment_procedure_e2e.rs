//! End-to-end coverage for procedure experiment target execution through Restate.

#![cfg(feature = "integration")]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_artifacts::validation::validate_for_status;
use moa_core::traits::Identity;
use moa_core::wire::artifacts::{
    ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest, ArtifactPublishResponse,
};
use moa_core::wire::experiments::{
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse,
};
use moa_core::wire::procedures::{
    ProcedureReviewDecisionKind, ProcedureReviewDecisionRequest, ProcedureReviewDecisionResponse,
    ProcedureRunStatus, ProcedureStatusRequest,
};
use moa_core::{
    types::action_policy::ActionRuleScope, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use moa_experiments::model::{ExperimentRunStatus, ExperimentScorecard, NewExperimentRun};
use moa_experiments::store::ExperimentStore;
use moa_orchestrator::workflows::experiment_run::ExperimentRunWorkflowRequest;
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::test_database_url;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_tenant_admin,
    register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
    test_user_identity, with_identity,
};

#[path = "support/mod.rs"]
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
        .env_remove("MOA_ANTHROPIC_API_KEY")
        .env_remove("MOA_OPENAI_API_KEY")
        .env_remove("MOA_GOOGLE_API_KEY")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for procedure experiment e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn procedure_experiment_links_queued_procedure_run() -> Result<()> {
    // Pins: procedure experiments start procedure runs and expose executed node projections.
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
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let published = import_and_publish_damaged_food_procedure(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
        )
        .await?;

        let run = run_procedure_experiment(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            &published,
        )
        .await?;
        assert_eq!(run.status, "accepted");
        assert_ne!(run.score_run_id, Uuid::nil());
        assert!(
            run.procedure_run_uid.is_none(),
            "procedure run should be linked by ExperimentRun after admission"
        );

        let experiment_status = wait_for_linked_procedure_experiment(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            run.run_uid,
        )
        .await?;
        let procedure_run_uid = experiment_status
            .procedure_run_uid
            .context("experiment status should expose linked procedure_run_uid")?;
        let procedure_status = procedure_status(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            procedure_run_uid,
        )
        .await?;

        assert_eq!(experiment_status.target_kind.as_deref(), Some("procedure"));
        assert_eq!(experiment_status.score_run_id, Some(run.score_run_id));
        assert_eq!(
            experiment_status.procedure_run_uid,
            Some(procedure_status.run_id)
        );
        assert_eq!(experiment_status.status, procedure_status.status);
        assert_eq!(procedure_status.status, "completed");
        assert_eq!(procedure_status.current_node_id.as_deref(), Some("done"));
        assert!(
            !procedure_status.node_runs.is_empty(),
            "procedure execution should persist node projections"
        );
        assert_eq!(
            node_ids(&procedure_status),
            vec!["start", "verify_evidence", "done"]
        );
        assert!(
            procedure_status
                .node_runs
                .iter()
                .all(|node_run| node_run.status == "completed"),
            "all deterministic procedure nodes should complete: {procedure_status:?}"
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
async fn procedure_experiment_run_awaits_pending_review_before_completing() -> Result<()> {
    // Pins: a procedure-backed experiment run (no plan/trials) does not finalize while its
    // procedure is still executing. The ExperimentRun/run invocation must stay in-flight while the
    // procedure is paused on a review node, keep the persisted run row `running`, and only finalize
    // a terminal `completed` status once the review is decided and the procedure reaches a terminal
    // state. The prior fire-and-forget `.send()` returned immediately while the procedure was still
    // running.
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
    let scope = ActionRuleScope::Tenant { tenant_id };
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        wait_for_orchestrator_live(&client, ports.health).await?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        let store = ExperimentStore::new(pool.clone());

        publish_review_gated_procedure(&pool, &scope).await?;

        let score_run_id = Uuid::now_v7();
        let target = review_gated_procedure_target();
        let variant = review_gated_variant();
        let run = store
            .insert_run(
                &scope,
                new_procedure_run(&identity, &target, &variant, score_run_id),
            )
            .await
            .context("seed procedure experiment run")?;

        let workflow_request = ExperimentRunWorkflowRequest {
            tenant_id,
            run_uid: run.run_uid,
            target,
            variant,
            plan_revision_uid: None,
            identity: identity.clone(),
            score_run_id,
            agent_revision_variants: Vec::new(),
        };

        // Invoke ExperimentRun/run in the background: with the fix it blocks until the procedure
        // reaches a terminal state, so the request stays in-flight until the review is decided.
        let mut run_task =
            spawn_experiment_run(&client, ingress, &identity, run.run_uid, &workflow_request);

        // Wait until the run has started and linked its procedure run.
        let procedure_run_uid = wait_for_run_procedure_link(&store, &scope, run.run_uid).await?;

        // Wait until the procedure has paused on its review node.
        let pending = wait_for_procedure_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            procedure_run_uid,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));

        // While the procedure is paused, the run must not have finalized: the persisted row is
        // still running and the run invocation is still in-flight.
        let blocked = store
            .load_run(&scope, run.run_uid)
            .await
            .context("load run while procedure is paused")?
            .context("run should exist while procedure is paused")?;
        assert_eq!(
            blocked.status,
            ExperimentRunStatus::Running,
            "run must remain running while its procedure is paused on review"
        );
        assert!(
            !run_task.is_finished(),
            "ExperimentRun/run must stay in-flight while the procedure is paused on review"
        );

        // Decide the review; the procedure resumes to completion and the run can finalize.
        let decision =
            decide_procedure_review(&client, ingress, &identity, tenant_id, procedure_run_uid)
                .await?;
        assert!(decision.accepted);

        let response = tokio::time::timeout(Duration::from_secs(60), &mut run_task)
            .await
            .context("timed out waiting for ExperimentRun/run to resolve after review")?
            .context("join ExperimentRun/run task")??;
        assert_eq!(
            response.status, "completed",
            "run should report a terminal completed status once the procedure completes"
        );
        assert_eq!(response.procedure_run_uid, Some(procedure_run_uid));

        let persisted = store
            .load_run(&scope, run.run_uid)
            .await
            .context("load persisted run after completion")?
            .context("run should exist after completion")?;
        assert_eq!(persisted.status, ExperimentRunStatus::Completed);
        assert!(
            persisted.completed_at.is_some(),
            "finalized run should record a completion timestamp"
        );

        pool.close().await;
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn experiments_run_denies_caller_without_tenant_operator() -> Result<()> {
    // Pins: Experiments/run rejects a caller who holds no Tenant:Operator grant with a 403
    // before any plan/target processing, instead of admitting an experiment run.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    // Caller carries the tenant but is never granted Tenant:Operator (or admin).
    let mut unauthorized = test_user_identity();
    unauthorized.tenant_id = tenant_id;
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let request = ExperimentRunRequest {
            tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
            name: "unauthorized-experiment-run".to_string(),
            plan_revision_uid: None,
            target: None,
            variant: None,
            scorecard: json!({}),
            score_run_id: None,
            idempotency_key: None,
            agent_revision_variants: Vec::new(),
        };

        let error = post_json_with_identity(
            &client,
            ingress,
            "Experiments",
            "run",
            &unauthorized,
            &request,
        )
        .await
        .expect_err("a caller without Tenant:Operator must not be admitted to Experiments/run");
        let message = error.to_string();
        assert!(
            message.contains("403")
                || message.contains("Forbidden")
                || message.contains("forbidden")
                || message.contains("authorization")
                || message.contains("authorized"),
            "expected a 403/authorization denial, got: {message}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn import_and_publish_damaged_food_procedure(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
) -> Result<ArtifactPublishResponse> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(
            Uuid::parse_str(storage_partition_id.as_str())
                .context("workspace id is tenant uuid")?,
        ),
    };
    let import_request = ArtifactImportRequest {
        scope,
        source_format: "yaml".to_string(),
        source_text: damaged_food_procedure_source().to_string(),
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

async fn run_procedure_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    published: &ArtifactPublishResponse,
) -> Result<ExperimentRunResponse> {
    let order_id = format!("ORD-{}", Uuid::now_v7());
    let request = ExperimentRunRequest {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
        name: "damaged-food-procedure-experiment".to_string(),
        plan_revision_uid: None,
        target: Some(json!({
            "kind": "procedure",
            "procedure_ref": "skill://damaged-food-replacement",
            "input": {
                "order_id": order_id,
                "damage_summary": "clear photo shows sauce leaked through the delivery bag",
                "customer_requested": "refund_or_replacement"
            },
            "session_id": null,
            "idempotency_key": format!("procedure-target-{}", Uuid::now_v7())
        })),
        variant: Some(json!({
            "name": "damaged-food-procedure",
            "model": null,
            "artifact_revision_uids": [published.revision_uid],
            "skill_refs": [],
            "procedure_ref": "skill://damaged-food-replacement",
            "metadata": { "lane": "procedure-experiment-e2e" }
        })),
        scorecard: json!({
            "score_names": ["procedure_started"],
            "evaluator_metadata": { "mode": "manual-or-later" }
        }),
        score_run_id: None,
        idempotency_key: Some(format!("experiment-procedure-{}", Uuid::now_v7())),
        agent_revision_variants: Vec::new(),
    };
    post_json_with_identity(client, ingress, "Experiments", "run", identity, &request)
        .await?
        .json::<ExperimentRunResponse>()
        .await
        .context("deserialize experiment run response")
}

async fn wait_for_linked_procedure_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    run_uid: Uuid,
) -> Result<ExperimentRunStatusResponse> {
    let request = ExperimentRunStatusRequest {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
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
            bail!("procedure experiment failed before linking a procedure run: {status:?}");
        }
        if status.procedure_run_uid.is_some() && status.status == "completed" {
            return Ok(status);
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for experiment {run_uid} to link a completed procedure run; last status: {last_status:?}"
    )
}

async fn procedure_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    run_id: Uuid,
) -> Result<ProcedureRunStatus> {
    let request = ProcedureStatusRequest {
        tenant_id: TenantId::from(
            Uuid::parse_str(storage_partition_id.as_str())
                .context("workspace id is tenant uuid")?,
        ),
        run_id,
    };
    post_json_with_identity(client, ingress, "Skills", "status", identity, &request)
        .await?
        .json::<ProcedureRunStatus>()
        .await
        .context("deserialize procedure status response")
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
    format!(
        "{}/restate/call/{service}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn node_ids(status: &ProcedureRunStatus) -> Vec<&str> {
    status
        .node_runs
        .iter()
        .map(|node_run| node_run.node_id.as_str())
        .collect()
}

async fn wait_for_orchestrator_live(client: &reqwest::Client, health_port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{health_port}/_health/live");
    let mut last_observation = "not yet checked".to_string();
    for _attempt in 0..120 {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_observation = format!("health returned {}", response.status());
            }
            Err(error) => {
                last_observation = error.to_string();
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!("timed out waiting for spawned orchestrator health at {url}: {last_observation}")
}

async fn publish_review_gated_procedure(pool: &PgPool, scope: &ActionRuleScope) -> Result<Uuid> {
    let source = review_gated_procedure_source();
    let document =
        ArtifactDocument::from_yaml(source).context("parse review-gated procedure artifact")?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        bail!(
            "review-gated procedure artifact should validate: {:?}",
            report.errors
        );
    }
    let registry = ArtifactRegistry::new(pool.clone());
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await
        .context("create review-gated procedure draft")?;
    let published = registry
        .publish_revision(scope, draft.revision_uid, &report)
        .await
        .context("publish review-gated procedure revision")?;
    Ok(published.revision_uid)
}

fn new_procedure_run(
    identity: &Identity,
    target: &Value,
    variant: &Value,
    score_run_id: Uuid,
) -> NewExperimentRun {
    NewExperimentRun {
        name: "review-gated procedure experiment".to_string(),
        target: serde_json::from_value(target.clone())
            .expect("procedure target fixture should parse"),
        variant: serde_json::from_value(variant.clone()).expect("variant fixture should parse"),
        scorecard: ExperimentScorecard {
            score_names: vec!["procedure_completed".to_string()],
            evaluator_metadata: json!({ "judge": "manual-or-later" }),
        },
        score_run_id,
        session_id: None,
        procedure_run_uid: None,
        artifact_revision_uids: Vec::new(),
        idempotency_key: Some(format!("procedure-run-{}", Uuid::now_v7())),
        created_by_identity: json!({
            "type": "operator",
            "id": identity.id.to_string(),
        }),
    }
}

fn review_gated_procedure_target() -> Value {
    json!({
        "kind": "procedure",
        "procedure_ref": "skill://review-gated-procedure",
        "input": {},
        "session_id": null,
        "idempotency_key": null
    })
}

fn review_gated_variant() -> Value {
    json!({
        "name": "review-gated-procedure",
        "model": null,
        "artifact_revision_uids": [],
        "skill_refs": [],
        "procedure_ref": "skill://review-gated-procedure",
        "metadata": { "lane": "procedure-run-review-e2e" }
    })
}

fn spawn_experiment_run(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    run_uid: Uuid,
    request: &ExperimentRunWorkflowRequest,
) -> JoinHandle<Result<ExperimentRunStatusResponse>> {
    let client = client.clone();
    let ingress = ingress.to_string();
    let identity = identity.clone();
    let request = request.clone();
    let service = format!("ExperimentRun/{run_uid}");
    tokio::spawn(async move {
        post_json_with_identity(&client, &ingress, &service, "run", &identity, &request)
            .await?
            .json::<ExperimentRunStatusResponse>()
            .await
            .context("deserialize backgrounded ExperimentRun/run response")
    })
}

async fn wait_for_run_procedure_link(
    store: &ExperimentStore,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> Result<Uuid> {
    for _attempt in 0..60 {
        if let Some(run) = store
            .load_run(scope, run_uid)
            .await
            .context("load run while waiting for procedure link")?
            && let Some(procedure_run_uid) = run.procedure_run_uid
        {
            return Ok(procedure_run_uid);
        }
        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for run {run_uid} to link a procedure run")
}

async fn wait_for_procedure_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
    expected: &str,
) -> Result<ProcedureRunStatus> {
    let request = ProcedureStatusRequest { tenant_id, run_id };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Skills", "status", identity, &request)
                .await?
                .json::<ProcedureRunStatus>()
                .await
                .context("deserialize procedure status response")?;
        if status.status == expected {
            return Ok(status);
        }
        if status.status == "failed" {
            bail!("procedure run {run_id} failed before reaching {expected}: {status:?}");
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for procedure run {run_id} to reach {expected}; last: {last_status:?}")
}

async fn decide_procedure_review(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<ProcedureReviewDecisionResponse> {
    let request = ProcedureReviewDecisionRequest {
        tenant_id,
        run_id,
        node_id: Some("gate".to_string()),
        decision: ProcedureReviewDecisionKind::Approved,
        reason: Some("approved in procedure run e2e".to_string()),
        output: Some(json!({ "approved": true })),
    };
    post_json_with_identity(
        client,
        ingress,
        "Skills",
        "decide_review",
        identity,
        &request,
    )
    .await?
    .json::<ProcedureReviewDecisionResponse>()
    .await
    .context("deserialize procedure review decision response")
}

fn review_gated_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: review-gated-procedure
  description: Procedure that pauses on an explicit review node.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
      nodes:
        - id: start
          kind: start
          ui:
            x: 80
            y: 120
        - id: gate
          kind: review
          input:
            prompt: Approve before completing the procedure.
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

fn damaged_food_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: damaged-food-replacement
  description: Procedure for refund, credit, or replacement when food arrives damaged.
  tags:
    - support
    - food-delivery
    - refund
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
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
        - id: done
          kind: end
          input:
            status: evidence_verified
          ui:
            x: 520
            y: 120
      edges:
        - id: start-to-verify
          from: start
          to: verify_evidence
        - id: verify-to-resolution
          from: verify_evidence
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

    bail!("published procedure had validation errors: {errors:?}")
}
