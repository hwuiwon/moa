mod support {
    pub mod grant_session_participant;
    pub mod grant_tenant_admin;
    pub mod restate_admin_url;
    pub mod restate_identity;
    pub mod restate_ingress_url;
    pub mod restate_lock;
    pub mod restate_ports;
    pub mod restate_register;
    pub mod session_get_events;
    pub mod session_init_vo;
    pub mod session_meta_fixture;

    pub mod restate_runtime {
        pub use super::grant_session_participant::grant_session_participant;
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

    pub mod session_store_service {
        pub use super::session_get_events::get_events_request;
        pub use super::session_init_vo::init_session_vo_request;
        pub use super::session_meta_fixture::test_session_meta;
    }
}

include!("workflow_execution_support/common.rs");
include!("workflow_execution_support/tool.rs");

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn workflow_tool_node_executes_through_tool_executor_service_e2e() -> Result<()> {
    // Pins: tool workflow nodes execute through policy and ToolExecutor, then resume the graph.
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
            tool_workflow_source(),
        )
        .await?;

        let run = start_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://tool-search-workflow",
            json!({}),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "search", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["search"]["is_error"].as_bool()),
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
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn workflow_agent_node_uses_session_turn_service_e2e() -> Result<()> {
    // Pins: workflow Agent nodes are deterministic-skill adapters into Session/TurnExecution.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("workflow-agent-script.json");
    let final_text = "The workflow agent turn completed through the existing session loop.";
    write_scripted_agent_fixture(&fixture_path, final_text)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut meta = test_session_meta(&format!("workflow-agent-{}", Uuid::now_v7()));
    meta.model = ModelId::new("scripted-loadtest");
    let tenant_id = meta.tenant_id;
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            agent_workflow_source(),
        )
        .await?;

        let run = start_workflow_with_session(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://agent-adapter-workflow",
            json!({}),
            Some(session_id),
        )
        .await?;
        let status =
            wait_for_completed_workflow(&client, ingress, &identity, tenant_id, run.run_id).await?;

        assert_eq!(status.current_node_id.as_deref(), Some("done"));
        assert_eq!(node_ids(&status), vec!["start", "agent", "done"]);
        assert_eq!(
            status
                .output
                .as_ref()
                .and_then(|value| value["agent"]["message"].as_str()),
            Some(final_text)
        );

        let events = fetch_events(&client, ingress, &identity, session_id).await?;
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                Event::UserMessage { text, .. }
                    if text == "Summarize the deterministic skill adapter status."
            )),
            "workflow agent node should persist the user message in the session event log"
        );
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text == final_text
            )),
            "workflow agent node should persist the brain response in the session event log"
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
async fn workflow_sub_agent_node_enforces_fanout_limits_service_e2e() -> Result<()> {
    // Pins: workflow SubAgent nodes reuse the existing root-session delegation fan-out limit.
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let meta = test_session_meta(&format!("workflow-sub-agent-{}", Uuid::now_v7()));
    let tenant_id = meta.tenant_id;
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, None)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;
        seed_active_session_children(&client, ingress, session_id).await?;
        import_and_publish_workflow(
            &client,
            ingress,
            &identity,
            tenant_id,
            sub_agent_workflow_source(),
        )
        .await?;

        let run = start_workflow_with_session(
            &client,
            ingress,
            &identity,
            tenant_id,
            "workflow://sub-agent-fanout-workflow",
            json!({}),
            Some(session_id),
        )
        .await?;
        let status =
            wait_for_workflow_status(&client, ingress, &identity, tenant_id, run.run_id, "failed")
                .await?;

        assert_eq!(status.current_node_id.as_deref(), Some("delegate"));
        assert_eq!(node_ids(&status), vec!["start", "delegate"]);
        assert_eq!(status.node_runs[1].status, "failed");
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("fan-out limit")),
            "workflow sub-agent node should fail through delegation fan-out validation: {status:?}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
