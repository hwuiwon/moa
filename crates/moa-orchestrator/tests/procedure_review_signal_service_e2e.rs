#[path = "support/mod.rs"]
mod support;

include!("procedure_execution_support/common.rs");
include!("procedure_execution_support/review_signal.rs");

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn procedure_review_node_pauses_and_resumes_service_e2e() -> Result<()> {
    // Pins: review nodes pause as pending_review and resume through Skills/decide_review.
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
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            review_procedure_source(),
        )
        .await?;

        let run = start_procedure(
            &client,
            ingress,
            &identity,
            tenant_id,
            "skill://review-gated-procedure",
            json!({}),
        )
        .await?;
        let pending = wait_for_procedure_status(
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
            decide_procedure_review(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(decision.accepted);
        assert_eq!(decision.status, "pending_review");

        let completed =
            wait_for_completed_procedure(&client, ingress, &identity, tenant_id, run.run_id)
                .await?;
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
async fn procedure_wait_signal_node_pauses_and_resumes_service_e2e() -> Result<()> {
    // Pins: wait_signal nodes keep the procedure body alive until Skills/signal resolves it.
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
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            wait_signal_procedure_source(),
        )
        .await?;

        let run = start_procedure(
            &client,
            ingress,
            &identity,
            tenant_id,
            "skill://signal-gated-procedure",
            json!({}),
        )
        .await?;
        let waiting = wait_for_procedure_status(
            &client, ingress, &identity, tenant_id, run.run_id, "running",
        )
        .await?;
        assert_eq!(waiting.current_node_id.as_deref(), Some("signal"));
        assert_eq!(node_ids(&waiting), vec!["start", "signal"]);

        let signal = signal_procedure(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(signal.accepted);
        assert_eq!(signal.status, "running");

        let completed =
            wait_for_completed_procedure(&client, ingress, &identity, tenant_id, run.run_id)
                .await?;
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
async fn procedure_cancel_resolves_paused_review_service_e2e() -> Result<()> {
    // Pins: Skills/cancel resolves the procedure execution cancel promise while blocked.
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
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            review_procedure_source(),
        )
        .await?;

        let run = start_procedure(
            &client,
            ingress,
            &identity,
            tenant_id,
            "skill://review-gated-procedure",
            json!({}),
        )
        .await?;
        let pending = wait_for_procedure_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));

        let cancelled =
            cancel_procedure(&client, ingress, &identity, tenant_id, run.run_id).await?;
        assert!(cancelled.cancelled);

        let status = wait_for_procedure_node_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            run.run_id,
            ProcedureNodeStatusExpectation {
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
