//! End-to-end workspace consolidation coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use moa_core::{ConfidenceLevel, MemoryScope, PageType, WikiPage, WorkspaceId};
use moa_orchestrator::objects::workspace::{
    WorkspaceApprovalPolicy, WorkspaceConfig, WorkspaceStatus,
};
use moa_orchestrator::services::memory_store::{ReadPageRequest, WritePageRequest};
use moa_orchestrator::workflows::consolidate::{ConsolidateReport, ConsolidateRequest};
use tempfile::TempDir;
use tokio::time::sleep;

use crate::support::restate_runtime::{OrchestratorPorts, reserve_orchestrator_ports};

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa:moa@127.0.0.1:5432/moa";

async fn register_deployment(endpoint_url: &str) -> Result<()> {
    for _attempt in 0..15 {
        let output = Command::new("restate")
            .args([
                "--connect-timeout",
                "10000",
                "--request-timeout",
                "30000",
                "deployments",
                "register",
                endpoint_url,
                "--yes",
            ])
            .output()
            .context("register deployment with local restate-server")?;

        if output.status.success() {
            return Ok(());
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("deployment registration did not succeed before retry budget was exhausted")
}

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
) -> Result<Child> {
    let postgres_url = std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string());

    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .env("POSTGRES_URL", postgres_url)
        .env("MOA_MEMORY_DIR", memory_dir.path())
        .env("MOA_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_DOCKER_ENABLED", "false")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
}

fn object_url(ingress: &str, workspace_id: &WorkspaceId, handler: &str) -> String {
    format!("{ingress}/Workspace/{workspace_id}/{handler}")
}

fn workflow_url(ingress: &str, workflow_id: &str) -> String {
    format!("{ingress}/Consolidate/{workflow_id}/run")
}

fn memory_page(title: &str, content: &str) -> WikiPage {
    let timestamp = Utc::now();
    WikiPage {
        path: None,
        title: title.to_string(),
        page_type: PageType::Topic,
        content: content.to_string(),
        created: timestamp,
        updated: timestamp,
        confidence: ConfidenceLevel::High,
        related: Vec::new(),
        sources: Vec::new(),
        tags: vec!["workspace".to_string()],
        auto_generated: false,
        last_referenced: timestamp,
        reference_count: 1,
        metadata: std::collections::HashMap::new(),
    }
}

#[tokio::test]
#[ignore = "requires a local restate-server and a reachable Postgres instance"]
async fn workspace_consolidation_round_trip_through_restate() -> Result<()> {
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);
    let ingress = "http://127.0.0.1:8080";
    let client = reqwest::Client::new();
    let workspace_id = WorkspaceId::new(format!(
        "workspace-consolidate-e2e-{}",
        uuid::Uuid::now_v7()
    ));
    let config = WorkspaceConfig {
        id: workspace_id.clone(),
        name: "Workspace Consolidate E2E".to_string(),
        consolidation_hour_utc: 2,
        approval_policy: WorkspaceApprovalPolicy::default(),
    };
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        register_deployment(endpoint_url.as_str()).await?;

        client
            .post(object_url(ingress, &workspace_id, "init"))
            .json(&config)
            .send()
            .await
            .context("initialize workspace VO")?
            .error_for_status()
            .context("workspace init should succeed")?;

        let initial_status = client
            .post(object_url(ingress, &workspace_id, "status"))
            .send()
            .await
            .context("read workspace status after init")?
            .error_for_status()
            .context("workspace status should succeed after init")?
            .json::<WorkspaceStatus>()
            .await
            .context("deserialize workspace status")?;
        assert!(
            initial_status.next_consolidation_at.is_some(),
            "expected the next consolidation to be scheduled after init"
        );

        client
            .post(format!("{ingress}/MemoryStore/write_page"))
            .json(&WritePageRequest {
                scope: MemoryScope::Workspace(workspace_id.clone()),
                path: "topics/architecture.md".into(),
                page: memory_page(
                    "Architecture",
                    "# Architecture\n\nThe deploy happened today.\n",
                ),
            })
            .send()
            .await
            .context("seed workspace memory page")?
            .error_for_status()
            .context("write_page should succeed")?;

        let target_date = Utc::now().date_naive();
        let workflow_id = format!("{}:{target_date}", workspace_id);
        let report = client
            .post(workflow_url(ingress, &workflow_id))
            .json(&ConsolidateRequest {
                workspace_id: workspace_id.clone(),
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

        assert_eq!(report.workspace_id, workspace_id);
        assert!(
            report.relative_dates_normalized >= 1 || report.pages_updated >= 1,
            "expected the workflow to rewrite the seeded page"
        );
        assert!(report.errors.is_empty(), "unexpected consolidation errors");

        let page = client
            .post(format!("{ingress}/MemoryStore/read_page"))
            .json(&ReadPageRequest {
                scope: MemoryScope::Workspace(workspace_id.clone()),
                path: "topics/architecture.md".into(),
            })
            .send()
            .await
            .context("read consolidated page")?
            .error_for_status()
            .context("read_page should succeed")?
            .json::<Option<WikiPage>>()
            .await
            .context("deserialize read_page response")?
            .context("expected seeded page to exist after consolidation")?;
        assert!(!page.content.contains("today"));

        let final_status = client
            .post(object_url(ingress, &workspace_id, "status"))
            .send()
            .await
            .context("read workspace status after consolidation")?
            .error_for_status()
            .context("workspace status should succeed after consolidation")?
            .json::<WorkspaceStatus>()
            .await
            .context("deserialize final workspace status")?;
        assert!(final_status.last_consolidation_at.is_some());
        assert!(final_status.next_consolidation_at.is_some());
        assert!(!final_status.consolidation_in_progress);
        assert!(final_status.pages_count >= 1);

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
