//! Stack-level e2e coverage for the agent `run_procedure` tool path.
//!
//! A scripted provider drives a real coordinator turn that emits a
//! `run_procedure` tool call, exercising the governed tool-invocation path
//! (crates/moa-orchestrator/src/tool_invocation/governed.rs) and its inline
//! procedure execution (crates/moa-orchestrator/src/procedure_tools.rs) end to
//! end: skill selection injects the tool schema, the governed path enforces the
//! selected procedure-capable set and the tenant action policy, and an allowed
//! call starts a durable `ProcedureExecution` run. These assert on the durable
//! session event log and the Skills run projection, not on prompt structure.
//!
//! Harness: reuses the restate-service e2e pattern (spawn `moa-orchestrator-bin`
//! against an ambient host restate-server + Postgres + OpenFGA, isolated per
//! run) shared by the other `procedure_*_service_e2e` tests, adding the scripted
//! provider override so a model turn is deterministic. Requires the orchestrator
//! binary to be built with the `provider-overrides` feature.

#[path = "support/mod.rs"]
mod support;

include!("procedure_execution_support/common.rs");
include!("procedure_execution_support/run_procedure_turn.rs");

const HAPPY_FINAL_TEXT: &str = "The procedure run has started.";
const REJECT_FINAL_TEXT: &str = "I could not start that procedure.";
const DENY_FINAL_TEXT: &str = "That action was not permitted.";

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and the provider-overrides feature"]
async fn run_procedure_tool_starts_durable_run_for_selected_skill_service_e2e() -> Result<()> {
    // Pins: a scripted model turn that emits run_procedure for the turn's selected
    // procedure-capable skill records ToolCall + ToolResult, the result carries a
    // run_id, and that durable run reaches a terminal (completed) state.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("happy-script.json");
    write_run_procedure_script(&fixture_path, "trivial-run-procedure", HAPPY_FINAL_TEXT)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let meta = pinned_procedure_session_meta(tenant_id, &identity, "skill://trivial-run-procedure");
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            &trivial_procedure_skill_source(
                "trivial-run-procedure",
                "Trivial start-to-end procedure the agent can run.",
            ),
        )
        .await?;

        let session_id = create_turn_session(&client, ingress, &identity, &meta).await?;
        let _turn_id = start_turn(
            &client,
            ingress,
            &identity,
            session_id,
            "Please run the trivial procedure now.",
        )
        .await?;
        let settled =
            wait_for_session_settled(&client, ingress, &identity, session_id, Duration::from_secs(120))
                .await?;
        let events = fetch_session_events(&client, ingress, &identity, session_id).await?;
        assert!(
            matches!(settled, SessionStatus::Paused | SessionStatus::Completed),
            "the turn should settle cleanly, got {settled:?}; events:\n{}",
            describe_events(&events)
        );
        assert_eq!(
            run_procedure_call_skill(&events).as_deref(),
            Some("trivial-run-procedure"),
            "the scripted model should emit a run_procedure tool call for the selected skill: {events:?}"
        );
        let (output, success) = run_procedure_tool_result(&events)
            .context("the event log must contain the run_procedure ToolCall and ToolResult")?;
        assert!(
            success,
            "run_procedure for the selected skill should succeed: {}",
            output.to_text()
        );
        let run_id = run_id_from_output(output)
            .context("a successful run_procedure output must carry a run_id")?;
        assert!(
            has_final_brain_response(&events, HAPPY_FINAL_TEXT),
            "the turn should finish with the scripted final response"
        );

        let status =
            wait_for_completed_procedure(&client, ingress, &identity, tenant_id, run_id).await?;
        assert_eq!(status.status, "completed", "the agent-started run should complete");
        assert_eq!(
            node_ids(&status),
            vec!["start", "done"],
            "the trivial procedure runs start then end"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and the provider-overrides feature"]
async fn run_procedure_tool_rejects_unselected_skill_without_starting_run_service_e2e() -> Result<()>
{
    // Pins: a run_procedure call for a published-but-unselected procedure skill is
    // rejected (naming the allowed selected skill) and creates no run, while the
    // same skill still runs when started directly through Skills/run.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("reject-script.json");
    // The scripted model targets the unselected skill (qualified reference form).
    write_run_procedure_script(
        &fixture_path,
        "skill://unselected-run-procedure",
        REJECT_FINAL_TEXT,
    )?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    // Pin the selected skill; the unselected skill is published but excluded.
    let meta =
        pinned_procedure_session_meta(tenant_id, &identity, "skill://selected-run-procedure");
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            &trivial_procedure_skill_source(
                "selected-run-procedure",
                "The selected procedure the turn pins.",
            ),
        )
        .await?;
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            &trivial_procedure_skill_source(
                "unselected-run-procedure",
                "A published procedure the turn does not select.",
            ),
        )
        .await?;

        let session_id = create_turn_session(&client, ingress, &identity, &meta).await?;
        let _turn_id = start_turn(
            &client,
            ingress,
            &identity,
            session_id,
            "Please run a procedure to resolve this.",
        )
        .await?;
        let settled = wait_for_session_settled(
            &client,
            ingress,
            &identity,
            session_id,
            Duration::from_secs(120),
        )
        .await?;
        let events = fetch_session_events(&client, ingress, &identity, session_id).await?;
        assert!(
            matches!(settled, SessionStatus::Paused | SessionStatus::Completed),
            "a model-correctable rejection should not fail the turn, got {settled:?}; events:\n{}",
            describe_events(&events)
        );
        assert_eq!(
            run_procedure_call_skill(&events).as_deref(),
            Some("skill://unselected-run-procedure"),
            "the scripted model should emit run_procedure for the unselected skill: {events:?}"
        );
        let (output, success) = run_procedure_tool_result(&events)
            .context("the event log must contain the run_procedure ToolCall and ToolResult")?;
        assert!(
            !success,
            "run_procedure for an unselected skill should be rejected: {}",
            output.to_text()
        );
        let rejection = output.to_text();
        assert!(
            rejection.contains("skill://unselected-run-procedure"),
            "the rejection should name the requested skill: {rejection}"
        );
        assert!(
            rejection.contains("skill://selected-run-procedure"),
            "the rejection should list the selected allowed skill: {rejection}"
        );
        assert!(
            rejection.contains("not among the selected"),
            "the rejection should explain the skill was not selected: {rejection}"
        );
        assert!(
            run_id_from_output(output).is_none(),
            "a rejected run_procedure must not start a run: {rejection}"
        );
        assert!(
            has_final_brain_response(&events, REJECT_FINAL_TEXT),
            "the turn should still finish after a model-correctable rejection"
        );

        // Control: the unselected skill IS a valid, runnable procedure when started
        // directly, so the rejection above prevented the run rather than the skill
        // being unrunnable.
        let control = start_procedure(
            &client,
            ingress,
            &identity,
            tenant_id,
            "skill://unselected-run-procedure",
            json!({}),
        )
        .await?;
        let control_status =
            wait_for_completed_procedure(&client, ingress, &identity, tenant_id, control.run_id)
                .await?;
        assert_eq!(
            control_status.status, "completed",
            "the unselected skill runs to completion when started directly"
        );
        assert_eq!(node_ids(&control_status), vec!["start", "done"]);

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and the provider-overrides feature"]
async fn run_procedure_tool_denied_by_action_policy_starts_no_run_service_e2e() -> Result<()> {
    // Pins: a tenant action-policy Deny rule on run_procedure denies the agent's
    // call (denied ToolResult, no run created) while the turn still completes.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("deny-script.json");
    write_run_procedure_script(
        &fixture_path,
        "skill://denied-run-procedure",
        DENY_FINAL_TEXT,
    )?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let meta = pinned_procedure_session_meta(tenant_id, &identity, "skill://denied-run-procedure");
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_skill(
            &client,
            ingress,
            &identity,
            tenant_id,
            &trivial_procedure_skill_source(
                "denied-run-procedure",
                "A selected procedure the tenant action policy denies.",
            ),
        )
        .await?;
        // Deny run_procedure at the tenant scope before the turn runs.
        upsert_run_procedure_rule(
            &client,
            ingress,
            &identity,
            tenant_id,
            ActionPolicyEffect::Deny,
        )
        .await?;

        let session_id = create_turn_session(&client, ingress, &identity, &meta).await?;
        let _turn_id = start_turn(
            &client,
            ingress,
            &identity,
            session_id,
            "Please run the denied procedure.",
        )
        .await?;
        let settled = wait_for_session_settled(
            &client,
            ingress,
            &identity,
            session_id,
            Duration::from_secs(120),
        )
        .await?;
        let events = fetch_session_events(&client, ingress, &identity, session_id).await?;
        assert!(
            matches!(settled, SessionStatus::Paused | SessionStatus::Completed),
            "a denied tool call should not fail the turn, got {settled:?}; events:\n{}",
            describe_events(&events)
        );
        assert_eq!(
            run_procedure_call_skill(&events).as_deref(),
            Some("skill://denied-run-procedure"),
            "the scripted model should emit run_procedure for the selected skill: {events:?}"
        );
        let (output, success) = run_procedure_tool_result(&events)
            .context("the event log must contain the run_procedure ToolCall and ToolResult")?;
        assert!(
            !success,
            "a run_procedure denied by action policy should be an error result: {}",
            output.to_text()
        );
        let denied = output.to_text();
        assert!(
            denied.contains("run_procedure"),
            "the denied output should name the tool: {denied}"
        );
        assert!(
            denied.contains("denied by action policy"),
            "the denied output should explain the action-policy denial: {denied}"
        );
        assert!(
            run_id_from_output(output).is_none(),
            "a denied run_procedure must not start a run: {denied}"
        );
        assert!(
            has_final_brain_response(&events, DENY_FINAL_TEXT),
            "the turn should still finish after a denied tool call"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
