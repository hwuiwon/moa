//! Offline release gates for sandbox-workspace rollout modes.

use moa_config::SandboxWorkspaceMode;
use moa_orchestrator::runtime::endpoint::{
    RegisteredDeployment, RegisteredService, expected_service_names, services_registered_for_mode,
};

fn deployment_for(mode: SandboxWorkspaceMode) -> RegisteredDeployment {
    RegisteredDeployment {
        id: format!("sandbox-workspace-{mode:?}"),
        services: expected_service_names(mode)
            .into_iter()
            .map(|name| RegisteredService {
                name: name.to_string(),
            })
            .collect(),
        uri: Some("http://127.0.0.1:10020".to_string()),
    }
}

#[test]
fn sandbox_workspace_rollout_disabled_stays_dark_offline() {
    // Pins: disabled mode starts neither maintenance nor admission and its
    // readiness contract must not wait for an intentionally unbound service.
    let mode = SandboxWorkspaceMode::Disabled;
    assert!(!mode.maintenance_enabled());
    assert!(!mode.admission_enabled());
    assert!(!expected_service_names(mode).contains(&"SandboxWorkspaces"));
    assert!(services_registered_for_mode(&[deployment_for(mode)], mode));
}

#[test]
fn sandbox_workspace_rollout_maintenance_keeps_cleanup_without_admission_offline() {
    // Pins: maintenance mode keeps reconciliation and cleanup deployable while
    // new workspace admission remains closed.
    let mode = SandboxWorkspaceMode::Maintenance;
    assert!(mode.maintenance_enabled());
    assert!(!mode.admission_enabled());
    assert!(expected_service_names(mode).contains(&"SandboxWorkspaces"));

    let dark_deployment = deployment_for(SandboxWorkspaceMode::Disabled);
    assert!(!services_registered_for_mode(&[dark_deployment], mode));
    assert!(services_registered_for_mode(&[deployment_for(mode)], mode));
}

#[test]
fn sandbox_workspace_rollout_admit_requires_workspace_service_and_enables_admission_offline() {
    // Pins: admission cannot open unless maintenance is active and the durable
    // workspace owner is present in the registered Restate deployment.
    let mode = SandboxWorkspaceMode::Admit;
    assert!(mode.maintenance_enabled());
    assert!(mode.admission_enabled());
    assert!(expected_service_names(mode).contains(&"SandboxWorkspaces"));

    let maintenance_services = deployment_for(SandboxWorkspaceMode::Maintenance);
    assert!(services_registered_for_mode(&[maintenance_services], mode));

    let dark_deployment = deployment_for(SandboxWorkspaceMode::Disabled);
    assert!(!services_registered_for_mode(&[dark_deployment], mode));
}
