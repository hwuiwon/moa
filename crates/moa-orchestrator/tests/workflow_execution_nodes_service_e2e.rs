mod support {
    pub mod grant_tenant_admin;
    pub mod restate_admin_url;
    pub mod restate_identity;
    pub mod restate_ingress_url;
    pub mod restate_lock;
    pub mod restate_ports;
    pub mod restate_register;

    pub mod restate_runtime {
        pub use super::grant_tenant_admin::grant_tenant_admin;
        pub use super::restate_admin_url::restate_admin_url;
        pub use super::restate_identity::{test_user_identity, with_identity};
        pub use super::restate_ingress_url::restate_ingress_url;
        pub use super::restate_lock::RESTATE_E2E_LOCK;
        pub use super::restate_ports::{
            OrchestratorPorts, deployment_endpoint_url, reserve_orchestrator_ports,
        };
        pub use super::restate_register::register_deployment;
    }
}

include!("workflow_execution_support/common.rs");
include!("workflow_execution_support/execution.rs");

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_execution_runs_deterministic_nodes_service_e2e() -> Result<()> {
    // Pins: `Workflows::run` starts `ArtifactWorkflowExecution` and persists node projections.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            deterministic_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://deterministic-routing",
            json!({ "decision": "approved" }),
        )
        .await?;
        assert_eq!(run.status, "queued");

        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert_eq!(status.run_id, run.run_id);
        assert_eq!(status.status, "completed");
        assert_eq!(status.current_node_id.as_deref(), Some("approved"));
        assert_eq!(status.output, Some(json!({ "route": "approved" })));
        assert_eq!(node_ids(&status), vec!["start", "route", "approved"]);
        assert!(
            status
                .node_runs
                .iter()
                .all(|node_run| node_run.status == "completed" && node_run.completed_at.is_some()),
            "all deterministic node runs should be terminal: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_parallel_join_executes_independent_tool_nodes_service_e2e() -> Result<()> {
    // Pins: workflow Parallel/Join topology persists visible branch node runs.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            parallel_join_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://parallel-join-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(
            node_ids(&status),
            vec!["start", "fanout", "left", "right", "join", "done"]
        );
        assert!(
            status
                .node_runs
                .iter()
                .all(|node_run| node_run.status == "completed"),
            "parallel workflow should complete every visible node: {status:?}"
        );
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["left"]["is_error"].as_bool()),
            Some(false)
        );
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["right"]["is_error"].as_bool()),
            Some(false)
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_loop_guard_stops_infinite_refinement_service_e2e() -> Result<()> {
    // Pins: workflow loop guards fail explicit back-edges instead of spinning indefinitely.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            loop_guard_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://loop-guard-workflow",
            json!({ "retry": true }),
        )
        .await?;
        let status =
            wait_for_workflow_status(&client, ingress, &identity, tenant_id, run.run_id, "failed")
                .await?;

        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("exceeded max iterations")),
            "loop guard should fail the workflow with a typed error: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
