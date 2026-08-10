//! Provider inventory reconciliation and quarantine evidence against Postgres.

use moa_config::CheckpointRetentionConfig;
use moa_core::types::{
    identifiers::{ProviderAccountId, TenantId},
    sandbox_workspace::{ProviderInventoryResource, ProviderInventoryResourceKind},
};
use sqlx::Row;

use super::sandbox_workspace_retention_db::{
    create_workspace, maintenance_fixture, pools, seed_account,
};

#[tokio::test]
#[ignore = "requires a fresh V58 database and distinct runtime/workspace-maintenance logins"]
async fn provider_inventory_drift_is_quarantined_then_resolved_only_after_clean_inventory_db() {
    // Pins: provider-account inventory is compared by the production
    // coordinator, unknown resources become durable maintenance-only quarantine
    // findings, and a later complete clean pass resolves them with audit proof.
    let (runtime, maintenance) = pools().await;
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_account(&runtime, account_id).await;
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
    let fixture = maintenance_fixture(
        &runtime,
        &maintenance,
        account_id,
        CheckpointRetentionConfig::default(),
    )
    .await;
    let resource_fingerprint = format!("sha256:unknown-{account_id}");
    let evidence_digest = format!("sha256:evidence-{account_id}");
    fixture
        .storage
        .set_inventory(vec![ProviderInventoryResource {
            kind: ProviderInventoryResourceKind::MutableFilesystem,
            provider_reference: format!("unknown-volume-{account_id}"),
            resource_fingerprint: resource_fingerprint.clone(),
            evidence_digest: evidence_digest.clone(),
            verified_owner: None,
        }])
        .await;

    let drift = fixture
        .coordinator
        .reconcile_provider_inventory_once()
        .await
        .expect("reconcile provider inventory with one unknown resource");
    assert_eq!(
        (drift.accounts, drift.resources, drift.unresolved_findings),
        (
            u64::try_from(active_account_count).expect("active account count is nonnegative"),
            1,
            1,
        )
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
        .reconcile_provider_inventory_once()
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
