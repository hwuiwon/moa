//! Provider inventory reconciliation and quarantine evidence against Postgres.

use moa_config::CheckpointRetentionConfig;
use moa_core::types::{
    identifiers::{ProviderAccountId, TenantId},
    sandbox_workspace::{
        ProviderInventoryOwner, ProviderInventoryResource, ProviderInventoryResourceKind,
    },
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::sandbox_workspace_retention_db::{
    create_workspace, maintenance_fixture, pools, seed_account,
};

async fn inventory_claim_generation(pool: &PgPool, account_id: ProviderAccountId) -> i64 {
    let mut transaction = pool.begin().await.expect("begin maintenance claim read");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(&mut *transaction)
        .await
        .expect("assume maintenance role for claim read");
    let generation = sqlx::query_scalar(
        "SELECT claim_generation FROM moa.sandbox_provider_inventory_claims \
         WHERE provider_account_id = $1 AND provider_account_generation = 1",
    )
    .bind(account_id)
    .fetch_one(&mut *transaction)
    .await
    .expect("load inventory claim generation");
    transaction
        .commit()
        .await
        .expect("commit maintenance claim read");
    generation
}

async fn inventory_claim_row_version(pool: &PgPool, account_id: ProviderAccountId) -> String {
    let mut transaction = pool.begin().await.expect("begin maintenance claim read");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(&mut *transaction)
        .await
        .expect("assume maintenance role for claim read");
    let version = sqlx::query_scalar(
        "SELECT xmin::text FROM moa.sandbox_provider_inventory_claims \
         WHERE provider_account_id = $1 AND provider_account_generation = 1",
    )
    .bind(account_id)
    .fetch_one(&mut *transaction)
    .await
    .expect("load inventory claim row version");
    transaction
        .commit()
        .await
        .expect("commit maintenance claim read");
    version
}

async fn expire_inventory_claim(pool: &PgPool, account_id: ProviderAccountId) {
    let mut transaction = pool
        .begin()
        .await
        .expect("begin maintenance claim mutation");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(&mut *transaction)
        .await
        .expect("assume maintenance role for claim mutation");
    sqlx::query(
        r#"
        UPDATE moa.sandbox_provider_inventory_claims
        SET claim_generation = claim_generation + 1,
            claim_owner = $2, claim_token = $3,
            claimed_at = now() - interval '2 minutes',
            claim_expires_at = now() - interval '1 minute'
        WHERE provider_account_id = $1 AND provider_account_generation = 1
          AND claim_token IS NULL AND last_succeeded_at IS NOT NULL
        "#,
    )
    .bind(account_id)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(&mut *transaction)
    .await
    .expect("simulate crashed maintenance owner with expired claim");
    transaction
        .commit()
        .await
        .expect("commit expired maintenance claim");
}

#[tokio::test]
#[ignore = "requires a fresh V60 database and distinct runtime/workspace-maintenance logins"]
async fn provider_inventory_claim_is_exclusive_and_recovers_after_owner_restart_db() {
    // Pins: two maintenance replicas cannot scan the same provider-account
    // generation concurrently, refreshing the queue does not rewrite an
    // unchanged claimed row, and an expired owner claim is reclaimed under a
    // strictly newer generation after restart.
    let (runtime, maintenance) = pools().await;
    let account_id = ProviderAccountId::new();
    seed_account(&runtime, account_id).await;
    sqlx::query(
        "UPDATE moa.sandbox_provider_accounts SET health = 'healthy' WHERE provider_account_id = $1",
    )
    .bind(account_id)
    .execute(&runtime)
    .await
    .expect("enable exact account inventory");
    let fixture = maintenance_fixture(
        &runtime,
        &maintenance,
        account_id,
        CheckpointRetentionConfig::default(),
    )
    .await;
    let (started, release) = fixture.storage.gate_next_inventory().await;
    let first_coordinator = fixture.coordinator.clone();
    let first = tokio::spawn(async move {
        first_coordinator
            .reconcile_claimed_provider_inventory_once(1)
            .await
    });
    started
        .await
        .expect("first replica reaches provider only after durable claim");
    let claimed_row_version = inventory_claim_row_version(&maintenance, account_id).await;
    let concurrent = fixture
        .coordinator
        .reconcile_claimed_provider_inventory_once(1)
        .await
        .expect("second replica sees no claimable account");
    assert_eq!(concurrent.accounts, 0);
    assert_eq!(
        inventory_claim_row_version(&maintenance, account_id).await,
        claimed_row_version,
        "refreshing the claim queue must not rewrite an unchanged provider row"
    );
    release
        .send(())
        .expect("release first inventory provider call");
    let completed = first
        .await
        .expect("first inventory task joins")
        .expect("first claimed inventory succeeds");
    assert_eq!(completed.accounts, 1);

    let generation_after_success = inventory_claim_generation(&maintenance, account_id).await;
    expire_inventory_claim(&maintenance, account_id).await;
    let stale_generation = inventory_claim_generation(&maintenance, account_id).await;
    assert!(stale_generation > generation_after_success);
    let recovered = fixture
        .coordinator
        .reconcile_claimed_provider_inventory_once(1)
        .await
        .expect("new replica recovers expired inventory claim");
    assert_eq!(recovered.accounts, 1);
    let recovered_generation = inventory_claim_generation(&maintenance, account_id).await;
    assert!(recovered_generation > stale_generation);
}

#[tokio::test]
#[ignore = "requires a fresh V58 database and distinct runtime/workspace-maintenance logins"]
async fn provider_inventory_drift_is_quarantined_then_resolved_only_after_clean_inventory_db() {
    // Pins: provider-account inventory is compared by the production
    // coordinator, unknown resources become durable maintenance-only quarantine
    // findings, durable workspace ownership is loaded only from the exact
    // account generation being scanned, and a later complete clean pass
    // resolves the findings with audit proof.
    let (runtime, maintenance) = pools().await;
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    let foreign_tenant_id = TenantId::new();
    let foreign_account_id = ProviderAccountId::new();
    seed_account(&runtime, account_id).await;
    seed_account(&runtime, foreign_account_id).await;
    sqlx::query(
        "UPDATE moa.sandbox_provider_accounts SET health = 'healthy' \
         WHERE provider_account_id = $1",
    )
    .bind(account_id)
    .execute(&runtime)
    .await
    .expect("enable only this test's account for fleet reconciliation");
    let active_account_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.sandbox_provider_accounts WHERE health <> 'disabled'",
    )
    .fetch_one(&runtime)
    .await
    .expect("count the exact fleet-wide provider-account scan set");
    let _workspace_id = create_workspace(&runtime, tenant_id, account_id).await;
    let foreign_workspace_id =
        create_workspace(&runtime, foreign_tenant_id, foreign_account_id).await;
    let fixture = maintenance_fixture(
        &runtime,
        &maintenance,
        account_id,
        CheckpointRetentionConfig::default(),
    )
    .await;
    let resource_fingerprint = format!("sha256:unknown-{account_id}");
    let evidence_digest = format!("sha256:evidence-{account_id}");
    let foreign_workspace_fingerprint = format!("sha256:foreign-{foreign_workspace_id}");
    fixture
        .storage
        .set_inventory(vec![
            ProviderInventoryResource {
                kind: ProviderInventoryResourceKind::MutableFilesystem,
                provider_reference: format!("unknown-volume-{account_id}"),
                resource_fingerprint: resource_fingerprint.clone(),
                evidence_digest: evidence_digest.clone(),
                verified_owner: None,
            },
            ProviderInventoryResource {
                kind: ProviderInventoryResourceKind::MutableFilesystem,
                provider_reference: format!("foreign-volume-{foreign_workspace_id}"),
                resource_fingerprint: foreign_workspace_fingerprint.clone(),
                evidence_digest: format!("sha256:foreign-evidence-{foreign_workspace_id}"),
                verified_owner: Some(ProviderInventoryOwner {
                    tenant_id: foreign_tenant_id,
                    workspace_id: foreign_workspace_id,
                    provisioning_operation_id: None,
                    writer_epoch: Some(0),
                    instance_generation: Some(0),
                }),
            },
        ])
        .await;

    let drift = fixture
        .coordinator
        .reconcile_claimed_provider_inventory_once(32)
        .await
        .expect("reconcile provider inventory with one unknown resource");
    assert_eq!(
        (drift.accounts, drift.resources, drift.unresolved_findings),
        (
            u64::try_from(active_account_count).expect("active account count is nonnegative"),
            2,
            2,
        )
    );
    let foreign_workspace_kind: String = sqlx::query_scalar(
        "SELECT finding_kind FROM moa.sandbox_provider_inventory_findings \
         WHERE provider_account_id = $1 AND provider_account_generation = 1 \
           AND resource_fingerprint = $2",
    )
    .bind(account_id)
    .bind(&foreign_workspace_fingerprint)
    .fetch_one(&runtime)
    .await
    .expect("load account-scoped foreign workspace finding");
    assert_eq!(
        foreign_workspace_kind, "unknown",
        "a workspace owned by another provider account is outside this account's durable inventory"
    );
    let quarantined = sqlx::query(
        "SELECT finding_kind, evidence_digest, quarantine_state, first_seen_at, last_seen_at, \
         resolved_at, resolved_by, resolution_evidence_digest \
         FROM moa.sandbox_provider_inventory_findings \
         WHERE provider_account_id = $1 AND provider_account_generation = 1 \
           AND resource_fingerprint = $2",
    )
    .bind(account_id)
    .bind(&resource_fingerprint)
    .fetch_one(&runtime)
    .await
    .expect("load durable quarantine finding");
    assert_eq!(
        quarantined
            .try_get::<String, _>("finding_kind")
            .expect("finding kind"),
        "unknown"
    );
    assert_eq!(
        quarantined
            .try_get::<String, _>("evidence_digest")
            .expect("evidence digest"),
        evidence_digest
    );
    assert_eq!(
        quarantined
            .try_get::<String, _>("quarantine_state")
            .expect("quarantine state"),
        "quarantined"
    );
    let first_seen = quarantined
        .try_get::<chrono::DateTime<chrono::Utc>, _>("first_seen_at")
        .expect("first seen timestamp");
    let last_seen = quarantined
        .try_get::<chrono::DateTime<chrono::Utc>, _>("last_seen_at")
        .expect("last seen timestamp");
    assert!(last_seen >= first_seen);
    assert_eq!(
        quarantined
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("resolved_at")
            .expect("unresolved timestamp"),
        None
    );

    fixture.storage.set_inventory(Vec::new()).await;
    let clean = fixture
        .coordinator
        .reconcile_claimed_provider_inventory_once(32)
        .await
        .expect("reconcile a complete clean provider inventory");
    assert_eq!(
        (clean.accounts, clean.resources, clean.unresolved_findings),
        (
            u64::try_from(active_account_count).expect("active account count is nonnegative"),
            0,
            0,
        )
    );
    let resolved = sqlx::query(
        "SELECT evidence_digest, quarantine_state, first_seen_at, last_seen_at, \
         resolved_at, resolved_by, resolution_evidence_digest \
         FROM moa.sandbox_provider_inventory_findings \
         WHERE provider_account_id = $1 AND provider_account_generation = 1 \
           AND resource_fingerprint = $2 AND finding_kind = 'unknown'",
    )
    .bind(account_id)
    .bind(&resource_fingerprint)
    .fetch_one(&runtime)
    .await
    .expect("load resolved inventory finding");
    assert_eq!(
        resolved
            .try_get::<String, _>("evidence_digest")
            .expect("original evidence digest"),
        evidence_digest,
        "resolution keeps the provider evidence that caused quarantine"
    );
    assert_eq!(
        resolved
            .try_get::<String, _>("quarantine_state")
            .expect("resolved quarantine state"),
        "resolved"
    );
    assert_eq!(
        resolved
            .try_get::<chrono::DateTime<chrono::Utc>, _>("first_seen_at")
            .expect("resolved first seen"),
        first_seen
    );
    assert_eq!(
        resolved
            .try_get::<chrono::DateTime<chrono::Utc>, _>("last_seen_at")
            .expect("resolved last seen"),
        last_seen
    );
    assert!(
        resolved
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("resolved_at")
            .expect("resolved timestamp")
            .is_some()
    );
    assert_eq!(
        resolved
            .try_get::<Option<String>, _>("resolved_by")
            .expect("resolution actor")
            .as_deref(),
        Some("workspace-maintenance")
    );
    assert!(
        resolved
            .try_get::<Option<String>, _>("resolution_evidence_digest")
            .expect("resolution digest")
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );

    runtime.close().await;
    maintenance.close().await;
}
