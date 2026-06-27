mod support {
    pub mod grant_tenant_admin;
    pub mod grant_tenant_operator;
    pub mod restate_admin_url;
    pub mod restate_identity;
    pub mod restate_ingress_url;
    pub mod restate_lock;
    pub mod restate_ports;
    pub mod restate_register;

    pub mod restate_runtime {
        pub use super::grant_tenant_admin::grant_tenant_admin;
        pub use super::grant_tenant_operator::grant_tenant_operator;
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
include!("workflow_execution_support/review_signal.rs");

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_review_node_pauses_and_resumes_service_e2e() -> Result<()> {
    // Pins: review nodes pause as pending_review and resume through Workflows/decide_review.
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
            review_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://review-gated-workflow",
            json!({}),
        )
        .await?;
        let pending = wait_for_workflow_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));
        assert_eq!(node_ids(&pending), vec!["start", "gate"]);
        assert_eq!(pending.node_runs[1].status, "pending_review");

        let decision =
            decide_workflow_review(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(decision.accepted);
        assert_eq!(decision.status, "pending_review");

        let completed =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&completed), vec!["start", "gate", "done"]);

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_wait_signal_node_pauses_and_resumes_service_e2e() -> Result<()> {
    // Pins: wait_signal nodes keep the workflow body alive until Workflows/signal resolves it.
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
    grant_tenant_operator(&identity, tenant_id).await?;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            wait_signal_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://signal-gated-workflow",
            json!({}),
        )
        .await?;
        let waiting = wait_for_workflow_status(
            &client, ingress, &identity, tenant_id, run.run_id, "running",
        )
        .await?;
        assert_eq!(waiting.current_node_id.as_deref(), Some("signal"));
        assert_eq!(node_ids(&waiting), vec!["start", "signal"]);

        let signal = signal_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(signal.accepted);
        assert_eq!(signal.status, "running");

        let completed =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert_eq!(completed.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&completed), vec!["start", "signal", "done"]);
        assert_eq!(
            completed
                .output
                .as_ref()
                .and_then(|value| value["signal"]["payload"]["ticket"].as_str()),
            Some("T-123")
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
async fn workflow_cancel_resolves_paused_review_service_e2e() -> Result<()> {
    // Pins: Workflows/cancel resolves the artifact workflow cancel promise while blocked.
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
            review_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://review-gated-workflow",
            json!({}),
        )
        .await?;
        let pending = wait_for_workflow_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));

        let cancelled = cancel_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(cancelled.cancelled);

        let status = wait_for_workflow_node_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            WorkflowNodeStatusExpectation {
                expected_run_status: "cancelled",
                node_id: "gate",
                expected_node_status: "cancelled",
            },
        )
        .await?;
        assert_eq!(status.current_node_id.as_deref(), Some("gate"));
        assert_eq!(node_ids(&status), vec!["start", "gate"]);
        assert_eq!(status.node_runs[1].status, "cancelled");

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
