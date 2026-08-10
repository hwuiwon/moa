//! Database startup fences for sandbox-workspace rollout modes.

use anyhow::Result;
use moa_config::{
    LocalHandProviderAccountConfig, MoaConfig, SandboxWorkspaceMode,
    SandboxWorkspaceQuotaRouteConfig,
};
use moa_core::types::identifiers::{ProviderAccountId, TenantId};
use moa_orchestrator::runtime::sandbox_workspace_rollout::{
    TenantQuotaBootstrapOutcome, bootstrap_accounts_and_quotas, validate_startup_state,
};

#[tokio::test]
async fn disabled_startup_rejects_live_durable_workspace_state_db() -> Result<()> {
    // Pins: a rollback cannot turn off the only cleanup owner while durable
    // workspace state remains, even though tenant RLS hides rows from ordinary reads.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let account_id = ProviderAccountId::new();
    let tenant_id = TenantId::new();
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint
        ) VALUES ($1, 1, 'local', 'fixture-a', 'local:fixture-a')
        "#,
    )
    .bind(account_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_workspaces (
            workspace_id, tenant_id, scope_kind, scope_session_id, scope_worker_id,
            provider, provider_account_id, provider_account_generation,
            durability_class, lifecycle_state
        ) VALUES (gen_random_uuid(), $1, 'worker', gen_random_uuid(), 'worker-1',
                  'local', $2, 1, 'portable_filesystem', 'ready')
        "#,
    )
    .bind(tenant_id)
    .bind(account_id)
    .execute(&pool)
    .await?;

    let error = validate_startup_state(&MoaConfig::default(), &pool)
        .await
        .expect_err("disabled startup must reject durable workspace state");
    assert!(error.to_string().contains("use maintenance to drain"));
    Ok(())
}

#[tokio::test]
async fn maintenance_bootstrap_is_idempotent_and_rejects_account_drift_db() -> Result<()> {
    // Pins: deployment-owned local account/quota bootstrap is repeatable, while
    // changing the persisted isolation identity under one account ID fails closed.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let account_id = ProviderAccountId::new();
    let tenant_id = TenantId::new();
    let mut config = MoaConfig::default();
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.local.provider_account = Some(LocalHandProviderAccountConfig {
        provider_account_id: account_id,
        generation: 2,
        isolation_cell: "fixture-a".to_string(),
    });
    config.sandbox_workspaces.quota_routes = vec![SandboxWorkspaceQuotaRouteConfig {
        tenant_id,
        provider_account_id: account_id,
        provider_account_generation: 2,
        max_workspaces: 4,
        max_active_hands: 2,
        max_checkpoints: 8,
        max_logical_bytes: 1_048_576,
    }];

    bootstrap_accounts_and_quotas(&config, &pool).await?;
    bootstrap_accounts_and_quotas(&config, &pool).await?;
    let mapping: (i64, String, String) = sqlx::query_as(
        "SELECT generation, provider, isolation_cell FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(mapping, (2, "local".to_string(), "fixture-a".to_string()));
    let limits: sqlx::types::Json<serde_json::Value> = sqlx::query_scalar(
        "SELECT configured_limits FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(limits.0["workspaces"], 4);

    config
        .local
        .provider_account
        .as_mut()
        .expect("fixture has local account")
        .isolation_cell = "drifted".to_string();
    let error = bootstrap_accounts_and_quotas(&config, &pool)
        .await
        .expect_err("persisted account identity drift must fail");
    assert!(
        error
            .to_string()
            .contains("bootstrap sandbox provider account")
    );
    Ok(())
}

#[tokio::test]
async fn maintenance_bootstrap_does_not_write_quota_during_active_tenant_purge_db() -> Result<()> {
    // Pins: restarting maintenance after the tenant destruction fence commits
    // must verify global provider state without rewriting tenant quota rows or
    // bypassing the fence installed by the purge owner.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let account_id = ProviderAccountId::new();
    let tenant_id = TenantId::new();
    let mut config = MoaConfig::default();
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.local.provider_account = Some(LocalHandProviderAccountConfig {
        provider_account_id: account_id,
        generation: 1,
        isolation_cell: "purge-restart-cell".to_string(),
    });
    config.sandbox_workspaces.quota_routes = vec![SandboxWorkspaceQuotaRouteConfig {
        tenant_id,
        provider_account_id: account_id,
        provider_account_generation: 1,
        max_workspaces: 4,
        max_active_hands: 2,
        max_checkpoints: 8,
        max_logical_bytes: 1_048_576,
    }];
    bootstrap_accounts_and_quotas(&config, &pool).await?;
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind("purge-restart-operation")
        .execute(&pool)
        .await?;

    config.sandbox_workspaces.quota_routes[0].max_workspaces = 9;
    let report = bootstrap_accounts_and_quotas(&config, &pool)
        .await
        .expect("maintenance restart must skip a quota write for the actively fenced tenant");
    assert_eq!(report.tenant_quotas.len(), 1);
    assert_eq!(
        report.tenant_quotas[0].outcome,
        TenantQuotaBootstrapOutcome::SkippedFenced
    );
    assert_eq!(
        report.fenced_tenants(),
        std::iter::once(tenant_id).collect()
    );
    let persisted: sqlx::types::Json<serde_json::Value> = sqlx::query_scalar(
        "SELECT configured_limits FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(persisted.0["workspaces"], 4);

    let provider_limits: sqlx::types::Json<serde_json::Value> = sqlx::query_scalar(
        "SELECT configured_limits FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(provider_limits.0["workspaces"], 9);

    let mut completed = pool.begin().await?;
    sqlx::query(
        r#"
        UPDATE moa.tenant_purge_operations
        SET status = 'relationally_committed',
            current_stage = 'complete',
            relationally_committed_at = now()
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id)
    .execute(&mut *completed)
    .await?;
    sqlx::query(
        r#"
        UPDATE moa.destruction_operation_fence
        SET status = 'committed', committed_at = now()
        WHERE tenant_id = $1 AND subject_id IS NULL
        "#,
    )
    .bind(tenant_id)
    .execute(&mut *completed)
    .await?;
    sqlx::query("DELETE FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&mut *completed)
        .await?;
    completed.commit().await?;

    let error = bootstrap_accounts_and_quotas(&config, &pool)
        .await
        .expect_err("stale quota config must not recreate a purged tenant");
    assert!(
        format!("{error:#}").contains("completed tenant purge"),
        "unexpected bootstrap error: {error:#}"
    );
    let recreated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(recreated, 0);
    Ok(())
}
