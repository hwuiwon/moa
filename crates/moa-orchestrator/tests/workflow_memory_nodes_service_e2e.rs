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
include!("workflow_execution_support/memory.rs");

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, AGE, and pgvector"]
async fn workflow_memory_read_respects_contact_scope_service_e2e() -> Result<()> {
    // Pins: workflow MemoryRead nodes execute through the scoped Memory service adapter.
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
    grant_tenant_operator(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            memory_read_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://memory-read-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "recall", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["recall"]["contact_id"].as_str()),
            Some("22222222-2222-2222-2222-222222222222")
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, AGE, and pgvector"]
async fn workflow_memory_write_records_scoped_fact_service_e2e() -> Result<()> {
    // Pins: workflow MemoryWrite nodes execute through graph-memory ingestion with provenance.
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
    grant_tenant_operator(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            memory_write_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://memory-write-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "remember", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["remember"]["contact_id"].as_str()),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert!(
            status
                .output
                .as_ref()
                .and_then(|value| value["remember"]["results"].as_array())
                .is_some_and(|results| results.len() == 1),
            "memory write should report one ingested document: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
