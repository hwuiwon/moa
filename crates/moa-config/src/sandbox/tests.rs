//! Unit tests for sandbox and durable workspace configuration.

use super::LocalHandProviderAccountConfig;
use super::checkpoint::SandboxCheckpointConfig;
use super::cloud::{
    CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig,
    ProviderSecretFileSelector,
};
use super::workspace::{
    SandboxWorkspaceCanaryConfig, SandboxWorkspaceMode, SandboxWorkspaceQuotaRouteConfig,
    SandboxWorkspacesConfig,
};
use crate::{
    KmsProviderKind, MoaConfig, ObjectStoreCredentialMode, OpenFgaConfig, SecurityProfile,
};
use moa_core::types::identifiers::{ProviderAccountId, TenantId};

#[test]
fn checkpoint_retention_rejects_zero_and_inconsistent_bounds() {
    // Pins: every replica rejects fail-open retention and deletion policy
    // before it may claim or remove checkpoint bytes.
    let mut zero = SandboxCheckpointConfig::default();
    zero.retention.gc_batch_size = 0;
    let zero_error = zero
        .validate()
        .expect_err("zero GC batch size must fail startup");
    assert!(matches!(
        zero_error,
        moa_core::error::MoaError::ConfigError(_)
    ));

    let mut inconsistent = SandboxCheckpointConfig::default();
    inconsistent.deletion.consistency_window_seconds = inconsistent.retention.claim_ttl_seconds + 1;
    let inconsistent_error = inconsistent
        .validate()
        .expect_err("absence proof window cannot outlive its GC claim");
    assert!(matches!(
        inconsistent_error,
        moa_core::error::MoaError::ConfigError(_)
    ));
}

#[test]
fn checkpoint_retention_accepts_bounded_default_policy() {
    // Pins: the checked-in policy is internally consistent and usable on
    // every replica without a hidden zero-means-unlimited convention.
    SandboxCheckpointConfig::default()
        .validate()
        .expect("bounded checkpoint defaults should validate");
}

#[test]
fn checkpoint_absence_windows_fit_current_tenant_purge_inactivity_budget() {
    // Pins: the current bounded workflow cannot admit three mandatory
    // separated waits that consume its entire inactivity budget before I/O.
    let mut config = SandboxCheckpointConfig::default();
    config.deletion.consistency_window_seconds = 120;
    assert!(config.validate().is_err());
    config.deletion.consistency_window_seconds = 119;
    config
        .validate()
        .expect("sub-budget separated waits remain admissible");
}

#[test]
fn sandbox_workspace_mode_defaults_disabled_and_rejects_unbounded_admission() {
    // Pins: an unset deployment stays dark, while `admit` cannot create a
    // workspace without an operator-selected canary and finite quota route.
    let default = SandboxWorkspacesConfig::default();
    assert_eq!(default.mode, SandboxWorkspaceMode::Disabled);

    let provider_account_id = ProviderAccountId::new();
    let tenant_id = TenantId::new();
    let admit = SandboxWorkspacesConfig {
        mode: SandboxWorkspaceMode::Admit,
        quota_routes: vec![SandboxWorkspaceQuotaRouteConfig {
            tenant_id,
            provider_account_id,
            provider_account_generation: 1,
            max_workspaces: 1,
            max_active_hands: 1,
            max_checkpoints: 1,
            max_logical_bytes: 1,
        }],
        ..SandboxWorkspacesConfig::default()
    };
    assert_eq!(
        admit
            .validate()
            .expect_err("admit without a canary must fail closed")
            .to_string(),
        "configuration error: sandbox workspaces admit mode requires an explicit canary route"
    );
}

#[test]
fn sandbox_workspace_maintenance_rejects_short_operation_retention() {
    // Pins: Restate cannot evict a durable operation owner before the
    // provider window and stale reconciliation claim can both converge.
    let config = SandboxWorkspacesConfig {
        mode: SandboxWorkspaceMode::Maintenance,
        maximum_operation_seconds: 600,
        operation_retention_seconds: 659,
        ..SandboxWorkspacesConfig::default()
    };

    assert!(
        config
            .validate()
            .expect_err("retention shorter than operation plus claim TTL must fail")
            .to_string()
            .contains("operation retention must cover")
    );
}

#[test]
fn sandbox_workspace_reconciliation_claim_outlives_reaper_interval() {
    // Pins: a zero or interval-sized reconciliation lease would let two
    // replicas observe the same provider operation concurrently, while an
    // excessively long lease would strand crash recovery.
    let mut config = SandboxWorkspacesConfig {
        mode: SandboxWorkspaceMode::Maintenance,
        quota_routes: vec![SandboxWorkspaceQuotaRouteConfig {
            tenant_id: TenantId::new(),
            provider_account_id: ProviderAccountId::new(),
            provider_account_generation: 1,
            max_workspaces: 1,
            max_active_hands: 1,
            max_checkpoints: 1,
            max_logical_bytes: 1,
        }],
        ..SandboxWorkspacesConfig::default()
    };
    config.reconciliation_claim_ttl_seconds = 10;
    assert!(config.validate().is_err());
    config.reconciliation_claim_ttl_seconds = 61;
    assert!(config.validate().is_err());
    config.reconciliation_claim_ttl_seconds = 60;
    config
        .validate()
        .expect("default reconciliation claim must exceed the reaper interval");
}

#[test]
fn sandbox_workspace_admit_rejects_canary_without_exact_quota_route() {
    // Pins: an allowlisted tenant still cannot enter an unlimited or
    // default-zero capacity route.
    let config = SandboxWorkspacesConfig {
        mode: SandboxWorkspaceMode::Admit,
        canary: Some(SandboxWorkspaceCanaryConfig {
            provider_account_id: ProviderAccountId::new(),
            provider_account_generation: 1,
            isolation_cell: "canary-a".to_string(),
            tenant_allowlist: vec![TenantId::new()],
        }),
        quota_routes: Vec::new(),
        ..SandboxWorkspacesConfig::default()
    };

    assert!(
        config
            .validate()
            .expect_err("every maintenance/admit deployment needs explicit quota routes")
            .to_string()
            .contains("requires explicit tenant/provider-account quota routes")
    );
}

fn sandbox_workspace_runtime_config() -> MoaConfig {
    let account_id = ProviderAccountId::new();
    let tenant_id = TenantId::new();
    let mut config = MoaConfig {
        security_profile: SecurityProfile::Cloud,
        ..MoaConfig::default()
    };
    config.object_store.credential_mode = ObjectStoreCredentialMode::WorkloadIdentity;
    config.database.maintenance_url =
        Some("postgres://moa_workspace_maintenance_login:test@db.example/moa".to_string());
    config.kms.provider = KmsProviderKind::Postgres;
    config.authz.openfga = Some(OpenFgaConfig {
        url: "https://openfga.example".to_string(),
        preshared_key: "test-only".to_string(),
        store_id: "store".to_string(),
        model_id: "model".to_string(),
        model_version: 7,
        timeout_ms: 2_000,
    });
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        fallback_providers: Vec::new(),
        provider_accounts: vec![CloudHandProviderAccountConfig {
            provider_account_id: account_id,
            generation: 1,
            provider: CloudHandProviderKind::E2b,
            isolation_cell: "canary-a".to_string(),
            api_origin: "https://api.e2b.dev".to_string(),
            toolbox_origin: None,
            sandbox_domain: Some("e2b.app".to_string()),
            default_runtime: Some("base".to_string()),
            project_fingerprint: Some("project:canary-a".to_string()),
            credential: ProviderSecretFileSelector {
                path: "/var/run/secrets/moa-hand-providers/e2b".into(),
                owner_uid: 10_001,
            },
        }],
    });
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.sandbox_workspaces.quota_routes = vec![SandboxWorkspaceQuotaRouteConfig {
        tenant_id,
        provider_account_id: account_id,
        provider_account_generation: 1,
        max_workspaces: 10,
        max_active_hands: 2,
        max_checkpoints: 100,
        max_logical_bytes: 1024 * 1024 * 1024,
    }];
    config
}

#[test]
fn sandbox_workspace_runtime_rejects_skip_fga_wrong_model_and_ephemeral_kms() {
    // Pins: every maintenance-capable process proves the exact authz/KMS
    // prerequisites before constructing a provider mutation owner.
    MoaConfig::default()
        .validate_sandbox_workspace_runtime(true)
        .expect("disabled rollout permits an intentionally dark local stack");
    let config = sandbox_workspace_runtime_config();
    config
        .validate_sandbox_workspace_runtime(false)
        .expect("complete maintenance prerequisites should validate");

    let mut missing_maintenance_database = config.clone();
    missing_maintenance_database.database.maintenance_url = None;
    assert!(
        missing_maintenance_database
            .validate_sandbox_workspace_runtime(false)
            .expect_err("workspace maintenance requires its own database login")
            .to_string()
            .contains("database.maintenance_url")
    );

    let mut shared_runtime_database = config.clone();
    shared_runtime_database.database.maintenance_url =
        Some(shared_runtime_database.database.url.clone());
    assert!(
        shared_runtime_database
            .validate_sandbox_workspace_runtime(false)
            .expect_err("workspace maintenance cannot reuse runtime credentials")
            .to_string()
            .contains("distinct from runtime")
    );

    assert!(
        config
            .validate_sandbox_workspace_runtime(true)
            .expect_err("skip FGA must fail")
            .to_string()
            .contains("MOA_SKIP_FGA")
    );

    let mut wrong_model = config.clone();
    wrong_model
        .authz
        .openfga
        .as_mut()
        .expect("fixture has OpenFGA")
        .model_version = 6;
    assert!(
        wrong_model
            .validate_sandbox_workspace_runtime(false)
            .expect_err("wrong model must fail")
            .to_string()
            .contains("exact OpenFGA model version 7")
    );

    let mut ephemeral = config;
    ephemeral.kms.provider = KmsProviderKind::Local;
    ephemeral.kms.allow_ephemeral = true;
    assert!(
        ephemeral
            .validate_sandbox_workspace_runtime(false)
            .expect_err("ephemeral KMS must fail")
            .to_string()
            .contains("durable Postgres KMS")
    );
}

#[test]
fn sandbox_workspace_runtime_accepts_explicit_local_account_canary() {
    // Pins: the deterministic local lane uses the same exact persisted
    // account/generation/isolation-cell fence without fake cloud credentials.
    let mut config = sandbox_workspace_runtime_config();
    let account_id = ProviderAccountId::new();
    let tenant_id = TenantId::new();
    config.security_profile = SecurityProfile::Local;
    config.cloud.hands = None;
    config.local.provider_account = Some(LocalHandProviderAccountConfig {
        provider_account_id: account_id,
        generation: 4,
        isolation_cell: "local-fixture-a".to_string(),
    });
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Admit;
    config.sandbox_workspaces.canary = Some(SandboxWorkspaceCanaryConfig {
        provider_account_id: account_id,
        provider_account_generation: 4,
        isolation_cell: "local-fixture-a".to_string(),
        tenant_allowlist: vec![tenant_id],
    });
    config.sandbox_workspaces.quota_routes = vec![SandboxWorkspaceQuotaRouteConfig {
        tenant_id,
        provider_account_id: account_id,
        provider_account_generation: 4,
        max_workspaces: 2,
        max_active_hands: 1,
        max_checkpoints: 8,
        max_logical_bytes: 1024 * 1024,
    }];

    config
        .validate_sandbox_workspace_runtime(false)
        .expect("explicit local account canary should validate");
    let resolved = config
        .sandbox_workspace_provider_account(account_id, 4)
        .expect("local account should resolve through the canonical seam");
    assert_eq!(resolved.provider, "local");
    assert_eq!(resolved.isolation_cell, "local-fixture-a");
    assert_eq!(resolved.project_fingerprint, None);
}

#[test]
fn sandbox_workspace_runtime_rejects_provider_fingerprint_and_canary_drift() {
    // Pins: restart/bootstrap accepts only the exact deployment-owned
    // provider account generation, cell, and immutable fingerprint.
    let mut missing_fingerprint = sandbox_workspace_runtime_config();
    missing_fingerprint
        .cloud
        .hands
        .as_mut()
        .expect("fixture has hands")
        .provider_accounts[0]
        .project_fingerprint = None;
    assert!(
        missing_fingerprint
            .validate_sandbox_workspace_runtime(false)
            .expect_err("missing fingerprint must fail")
            .to_string()
            .contains("immutable project fingerprints")
    );

    let mut mismatched_canary = sandbox_workspace_runtime_config();
    let account_id = mismatched_canary
        .cloud
        .hands
        .as_ref()
        .expect("fixture has hands")
        .provider_accounts[0]
        .provider_account_id;
    mismatched_canary.sandbox_workspaces.canary = Some(SandboxWorkspaceCanaryConfig {
        provider_account_id: account_id,
        provider_account_generation: 2,
        isolation_cell: "canary-a".to_string(),
        tenant_allowlist: vec![TenantId::new()],
    });
    assert!(
        mismatched_canary
            .validate_sandbox_workspace_runtime(false)
            .expect_err("canary generation drift must fail")
            .to_string()
            .contains("exact provider-account bootstrap mapping")
    );
}
