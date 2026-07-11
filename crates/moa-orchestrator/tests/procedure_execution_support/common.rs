// Common end-to-end procedure execution support.

use std::process::{Child, Command, Stdio};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::wire::artifacts::{
    ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest, ArtifactPublishResponse,
};
use moa_core::wire::procedures::{
    ProcedureRunRequest, ProcedureRunResponse, ProcedureRunStatus, ProcedureStatusRequest,
};
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
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
        .env_remove("MOA_ANTHROPIC_API_KEY")
        .env_remove("MOA_OPENAI_API_KEY")
        .env_remove("MOA_GOOGLE_API_KEY")
        .env_remove("MOA_COHERE_API_KEY")
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
        .context("spawn moa-orchestrator binary for procedure execution e2e")
}

async fn import_and_publish_skill(
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

async fn start_procedure(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    procedure_ref: &str,
    input: Value,
) -> Result<ProcedureRunResponse> {
    let request = ProcedureRunRequest {
        tenant_id,
        procedure_ref: procedure_ref.to_string(),
        input,
        session_id: None,
        idempotency_key: Some(format!("procedure-{}", Uuid::now_v7())),
    };
    post_json_with_identity(client, ingress, "Skills", "run", identity, &request)
        .await?
        .json::<ProcedureRunResponse>()
        .await
        .context("deserialize procedure run response")
}

async fn wait_for_completed_procedure(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<ProcedureRunStatus> {
    wait_for_procedure_status(client, ingress, identity, tenant_id, run_id, "completed").await
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
            bail!("procedure run failed before reaching {expected}: {status:?}");
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for procedure run {run_id} to reach {expected}; last status: {last_status:?}"
    )
}

#[allow(clippy::too_many_arguments)]
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
    format!("{}/restate/call/{service}/{handler}", ingress.trim_end_matches('/'))
}

fn node_ids(status: &ProcedureRunStatus) -> Vec<&str> {
    status
        .node_runs
        .iter()
        .map(|node_run| node_run.node_id.as_str())
        .collect()
}

fn assert_validation_report_has_no_errors(report: &Value) -> Result<()> {
    let Some(errors) = report.get("errors").and_then(Value::as_array) else {
        bail!("validation report did not include an errors array: {report}");
    };
    if errors.is_empty() {
        return Ok(());
    }

    bail!("published skill had validation errors: {errors:?}")
}
