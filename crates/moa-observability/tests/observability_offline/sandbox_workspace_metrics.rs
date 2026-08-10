//! Offline contract checks for durable sandbox workspace metrics.

use std::time::Duration;

use metrics_exporter_prometheus::PrometheusBuilder;
use moa_core::types::sandbox_workspace::{SandboxWorkspaceState, WorkspaceCapacityDimension};
use moa_observability::{
    SandboxStorageResourceMetricState, SandboxWorkspaceCheckpointOperation,
    SandboxWorkspaceInventoryDrift, SandboxWorkspaceLifecycleOperation,
    SandboxWorkspaceMetricResult, SandboxWorkspaceProviderKind, SandboxWorkspaceQuotaDecision,
    record_sandbox_storage_resource_state, record_sandbox_workspace_checkpoint,
    record_sandbox_workspace_inventory_drift, record_sandbox_workspace_lifecycle,
    record_sandbox_workspace_quota_decision, record_sandbox_workspace_quota_utilization,
    record_sandbox_workspace_reaper, record_sandbox_workspace_state,
};

#[test]
fn sandbox_workspace_metrics_export_only_bounded_operational_dimensions_offline() {
    // Pins: the production metric API exposes lifecycle, fleet, quota, reaper,
    // checkpoint, and inventory signals without accepting tenant/workspace/account
    // identities or any path/content/secret label values.
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        record_sandbox_workspace_lifecycle(
            SandboxWorkspaceProviderKind::Daytona,
            SandboxWorkspaceLifecycleOperation::Commit,
            SandboxWorkspaceMetricResult::Succeeded,
            Duration::from_millis(250),
        );
        record_sandbox_workspace_state(
            SandboxWorkspaceProviderKind::Daytona,
            SandboxWorkspaceState::Active,
            3,
        );
        record_sandbox_storage_resource_state(
            SandboxWorkspaceProviderKind::Daytona,
            SandboxStorageResourceMetricState::Attached,
            3,
        );
        record_sandbox_workspace_quota_decision(
            WorkspaceCapacityDimension::Volumes,
            SandboxWorkspaceQuotaDecision::Rejected,
        );
        record_sandbox_workspace_quota_utilization(WorkspaceCapacityDimension::Volumes, 0.75);
        record_sandbox_workspace_reaper(true, Duration::from_secs(2), 4, Duration::from_secs(30));
        record_sandbox_workspace_checkpoint(
            SandboxWorkspaceProviderKind::E2b,
            SandboxWorkspaceCheckpointOperation::Restore,
            SandboxWorkspaceMetricResult::Succeeded,
            4096,
            Duration::from_secs(1),
        );
        record_sandbox_workspace_inventory_drift(
            SandboxWorkspaceProviderKind::Daytona,
            SandboxWorkspaceInventoryDrift::WrongOwner,
            1,
        );
    });

    let rendered = handle.render();
    for family in [
        "moa_sandbox_workspace_lifecycle_total",
        "moa_sandbox_workspace_lifecycle_duration_seconds",
        "moa_sandbox_workspace_state",
        "moa_sandbox_workspace_storage_resource_state",
        "moa_sandbox_workspace_quota_decisions_total",
        "moa_sandbox_workspace_quota_utilization_ratio",
        "moa_sandbox_workspace_reaper_ready",
        "moa_sandbox_workspace_reaper_heartbeat_age_seconds",
        "moa_sandbox_workspace_reaper_backlog",
        "moa_sandbox_workspace_reaper_oldest_work_age_seconds",
        "moa_sandbox_workspace_checkpoint_bytes_total",
        "moa_sandbox_workspace_checkpoint_duration_seconds",
        "moa_sandbox_workspace_inventory_drift",
    ] {
        assert!(
            rendered.contains(family),
            "workspace scrape must contain {family}:\n{rendered}"
        );
    }

    for forbidden_label in [
        "tenant_id",
        "workspace_id",
        "provider_account_id",
        "resource_id",
        "path",
        "content",
        "secret",
    ] {
        assert!(
            !rendered.contains(forbidden_label),
            "workspace metrics must not expose `{forbidden_label}`: {rendered}"
        );
    }
    assert!(rendered.contains("provider_kind=\"daytona\""));
    assert!(rendered.contains("classification=\"wrong_owner\""));
    assert!(rendered.contains("decision=\"rejected\""));
}
