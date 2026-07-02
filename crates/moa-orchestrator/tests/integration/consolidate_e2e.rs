//! End-to-end tenant consolidation coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use chrono::Utc;
use moa_core::TenantId;
use moa_orchestrator::objects::tenant::{TenantConfig, TenantStatus};
use moa_orchestrator::workflows::consolidate::{ConsolidateReport, ConsolidateRequest};
use moa_test_support::postgres::test_database_url;
use tempfile::TempDir;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, register_deployment,
    reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
};

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
) -> Result<Child> {
    let postgres_url = test_database_url();

    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", postgres_url)
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
}

fn object_url(ingress: &str, tenant_id: TenantId, handler: &str) -> String {
    format!(
        "{}/restate/call/Tenant/{tenant_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn workflow_url(ingress: &str, workflow_id: &str) -> String {
    format!(
        "{}/restate/call/Consolidate/{workflow_id}/run",
        ingress.trim_end_matches('/')
    )
}

#[tokio::test]
#[ignore = "requires a local restate-server and a reachable Postgres instance"]
async fn tenant_consolidation_round_trip_through_restate() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let config = TenantConfig {
        id: tenant_id,
        name: "Tenant Consolidate E2E".to_string(),
        consolidation_hour_utc: 2,
    };
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        client
            .post(object_url(ingress, tenant_id, "init"))
            .json(&config)
            .send()
            .await
            .context("initialize tenant VO")?
            .error_for_status()
            .context("tenant init should succeed")?;

        let initial_status = client
            .post(object_url(ingress, tenant_id, "status"))
            .send()
            .await
            .context("read tenant status after init")?
            .error_for_status()
            .context("tenant status should succeed after init")?
            .json::<TenantStatus>()
            .await
            .context("deserialize tenant status")?;
        assert!(
            initial_status.next_consolidation_at.is_some(),
            "expected the next consolidation to be scheduled after init"
        );

        let target_date = Utc::now().date_naive();
        let workflow_id = format!(
            "{}:{target_date}:manual-{}",
            tenant_id,
            uuid::Uuid::now_v7()
        );
        let report = client
            .post(workflow_url(ingress, &workflow_id))
            .json(&ConsolidateRequest {
                tenant_id,
                target_date,
            })
            .send()
            .await
            .context("run consolidate workflow")?
            .error_for_status()
            .context("consolidate workflow should succeed")?
            .json::<ConsolidateReport>()
            .await
            .context("deserialize consolidate report")?;

        assert_eq!(report.tenant_id, tenant_id);
        assert_eq!(report.relative_dates_normalized, 0);
        assert_eq!(report.records_updated, 0);
        assert!(report.errors.is_empty(), "unexpected consolidation errors");

        let final_status = client
            .post(object_url(ingress, tenant_id, "status"))
            .send()
            .await
            .context("read tenant status after consolidation")?
            .error_for_status()
            .context("tenant status should succeed after consolidation")?
            .json::<TenantStatus>()
            .await
            .context("deserialize final tenant status")?;
        assert!(final_status.last_consolidation_at.is_some());
        assert!(final_status.next_consolidation_at.is_some());
        assert!(!final_status.consolidation_in_progress);
        assert_eq!(final_status.pages_count, 0);

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
