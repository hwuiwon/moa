//! Service-level coverage for tenant-keyed durable purge execution.

use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};
use moa_core::types::identifiers::TenantId;
use moa_test_support::postgres::test_database_url;
use moa_wire::tenants::{
    TenantPurgeRequest, TenantPurgeStatus, TenantPurgeStatusRequest, TenantPurgeStatusResponse,
};
use tempfile::TempDir;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, register_deployment,
    reserve_orchestrator_ports, restate_admin_url, restate_ingress_url, test_user_identity,
    with_identity,
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
        .env("MOA_SECURITY_PROFILE", "local")
        .env("MOA_KMS_ALLOW_EPHEMERAL", "true")
        .env("MOA_RUNTIME_CACHE_BACKEND", "redis")
        .env(
            "MOA_RUNTIME_CACHE_REDIS_URL",
            std::env::var("MOA_RUNTIME_CACHE_REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:10051".to_string()),
        )
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn orchestrator for tenant purge e2e")
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and Valkey"]
async fn tenant_purge_commits_once_and_preserves_final_inverse_tuples_service_e2e() -> Result<()> {
    // Pins: one tenant-keyed workflow commits inverse tuples with deletion, reports durable status, and reaches analytics_purged.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let user_id = Uuid::new_v4();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        register_deployment(&restate_admin_url(), &endpoint_url).await?;
        let pool = sqlx::PgPool::connect(&test_database_url())
            .await
            .context("connect tenant purge e2e database")?;
        sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'purge e2e')")
            .bind(tenant_id.0)
            .bind(format!("purge-{tenant_id}"))
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO users (id, tenant_id, email, active) VALUES ($1, $2, $3, TRUE)",
        )
        .bind(user_id)
        .bind(tenant_id.0)
        .bind(format!("{user_id}@example.test"))
        .execute(&pool)
        .await?;

        let path = format!("/TenantPurge/{tenant_id}/run");
        let first: TenantPurgeStatusResponse = call(
            &client,
            &ingress,
            &identity,
            &path,
            &TenantPurgeRequest { tenant_id },
        )
        .await?;
        let status: TenantPurgeStatusResponse = call(
            &client,
            &ingress,
            &identity,
            &format!("/TenantPurge/{tenant_id}/status"),
            &TenantPurgeStatusRequest { tenant_id },
        )
        .await?;

        assert_eq!(status, first);
        assert_eq!(status.status, TenantPurgeStatus::AnalyticsPurged);
        let fence_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.tenant_purge_operations WHERE tenant_id = $1 AND status = 'relationally_committed'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await?;
        let destruction_fence_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.destruction_operation_fence WHERE tenant_id = $1 AND subject_id IS NULL AND status = 'committed'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await?;
        let product_count: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM tenants WHERE id = $1) + (SELECT count(*) FROM users WHERE tenant_id = $1)",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await?;
        let inverse_tuple_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM authz_outbox WHERE tenant_id = $1 AND op = 'delete'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await?;
        assert_eq!(fence_count, 1);
        assert_eq!(destruction_fence_count, 1);
        assert_eq!(product_count, 0);
        assert_eq!(inverse_tuple_count, 3);
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    result
}

async fn call<Req, Resp>(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    path: &str,
    request: &Req,
) -> Result<Resp>
where
    Req: serde::Serialize + ?Sized,
    Resp: serde::de::DeserializeOwned,
{
    let response = with_identity(
        client
            .post(format!(
                "{}/restate/call{path}",
                ingress.trim_end_matches('/')
            ))
            .json(request),
        identity,
    )
    .send()
    .await
    .context("call tenant purge workflow")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read tenant purge response")?;
    if !status.is_success() {
        bail!("tenant purge workflow returned {status}: {body}");
    }
    serde_json::from_str(&body).context("decode tenant purge response")
}
