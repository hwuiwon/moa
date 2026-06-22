//! Combined behavior-lab simulation E2E coverage through Restate.

#![cfg(feature = "integration")]

use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_core::{
    ActionRuleScope, Event, EventRange, EventRecord, ScopeContext, ScopedConn, SessionId, TenantId,
    WorkspaceId,
    traits::Identity,
    wire::{
        ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest,
        ArtifactPublishResponse, ExperimentRunRequest, ExperimentRunResponse,
        ExperimentRunStatusRequest, ExperimentRunStatusResponse, ExperimentScoresRequest,
        ExperimentScoresResponse, ExperimentTrialStatusRequest, ExperimentTrialStatusResponse,
        ExperimentTrialSummary, ExperimentTrialsRequest, ExperimentTrialsResponse,
        SkillImportRequest, SkillImportResponse, SkillPackageDocument, SkillPackageDocumentFile,
        WorkflowRunRequest, WorkflowRunResponse, WorkflowRunStatus, WorkflowStatusRequest,
    },
};
use moa_test_support::postgres::test_database_url;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::{
    restate_runtime::{
        OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_tenant_admin,
        register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
        test_user_identity, with_identity,
    },
    session_store_service::get_events_request,
};

mod support;

const SUPPORT_SKILL_PATH: &str = ".moa/skills/delivery-support/SKILL.md";
const SUPPORT_SKILL_PROVIDER_ID: &str = "behavior-lab-read-delivery-support";

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    provider_override_fixture: &Path,
) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", provider_override_fixture.display()),
        )
        .env("RUST_LOG", "info")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for behavior-lab simulation e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn damaged_food_plan_links_trial_session_workflow_skill_and_score_runs() -> Result<()> {
    // Pins: a behavior-lab plan drives a damaged-food simulator trial through skills and same-session workflow association.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir
        .path()
        .join("damaged-food-behavior-lab-script.json");
    write_damaged_food_fixture(&fixture_path)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let workspace_id = WorkspaceId::new(tenant_id.to_string());
    let scope = ActionRuleScope::Tenant { tenant_id };
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_support_skill(&client, ingress, &identity, &workspace_id).await?;

        let workflow = import_and_publish_artifact(
            &client,
            ingress,
            &identity,
            &workspace_id,
            damaged_food_workflow_source(),
        )
        .await?;
        let plan = import_and_publish_artifact(
            &client,
            ingress,
            &identity,
            &workspace_id,
            damaged_food_plan_source(),
        )
        .await?;

        let run = run_plan_experiment(
            &client,
            ingress,
            &identity,
            &workspace_id,
            "damaged-food-behavior-lab",
            plan.revision_uid,
        )
        .await?;
        assert_eq!(run.status, "accepted");
        assert_ne!(run.score_run_id, Uuid::nil());
        assert!(run.session_id.is_none());
        assert!(run.workflow_run_uid.is_none());

        let status =
            wait_for_run_status(&client, ingress, &identity, &workspace_id, run.run_uid, |status| {
                status.status == "completed"
            })
            .await?;
        assert_eq!(status.status, "completed");
        assert_eq!(status.score_run_id, Some(run.score_run_id));

        let trials = list_trials(&client, ingress, &identity, &workspace_id, run.run_uid).await?;
        assert_eq!(trials.trials.len(), 1);
        let trial = &trials.trials[0];
        assert_eq!(trial.run_uid, run.run_uid);
        assert_eq!(trial.status, "completed");
        assert_eq!(trial.target_kind, "agent_loop");
        assert_eq!(trial.variant_key, "support-agent");
        assert_eq!(
            trial.scenario_id.as_deref(),
            Some("damaged-food-unclear-photo")
        );
        assert_eq!(trial.turn_count, 2);
        assert_eq!(trial.stop_reason.as_deref(), Some("max_turns"));
        assert_ne!(trial.score_run_id, Uuid::nil());
        assert!(trial.workflow_run_uid.is_none());
        let session_id = trial
            .session_id
            .context("damaged-food trial should link the target session")?;

        let trial_status = trial_status(
            &client,
            ingress,
            &identity,
            &workspace_id,
            trial.trial_uid,
        )
        .await?;
        assert_trial_status_matches_summary(&trial_status, trial);

        let events =
            wait_for_session_messages(&client, ingress, &identity, session_id, 2, 2).await?;
        assert_eq!(
            user_message_texts(&events),
            vec![
                "The photo is blurry, but it looks like soup leaked through the delivery bag.",
                "Order FOOD-42 arrived with soup pooled under the container and sauce on every item.",
            ]
        );
        assert_eq!(
            brain_response_texts(&events),
            vec![
                "I can help, but the photo is unclear. Please describe the damage and share the order id before I recommend a replacement.",
                "Thanks for the clearer description. The damaged-food workflow can be associated with FOOD-42 for replacement review.",
            ]
        );
        assert!(
            saw_successful_skill_file_read(&events),
            "expected target to read the imported delivery support skill; observed events: {}",
            summarize_events(&events)
        );

        let workflow_run =
            run_workflow_for_session(&client, ingress, &identity, &workspace_id, session_id).await?;
        assert_eq!(workflow_run.status, "queued");
        let workflow_status =
            workflow_status(&client, ingress, &identity, &workspace_id, workflow_run.run_id)
                .await?;
        assert_eq!(workflow_status.run_id, workflow_run.run_id);
        assert_eq!(workflow_status.session_id, Some(session_id));
        assert_eq!(workflow_status.status, "queued");
        assert!(workflow_status.node_runs.is_empty());

        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        assert_score_run_parent(&pool, &scope, run.score_run_id, "experiment_run").await?;
        assert_score_run_parent(&pool, &scope, trial.score_run_id, "experiment_trial").await?;
        assert_no_analytics_scores(&pool, &workspace_id, &[run.score_run_id, trial.score_run_id])
            .await?;
        assert_no_learning_candidates(&pool, &scope, &workspace_id).await?;

        let scores = experiment_scores(&client, ingress, &identity, &workspace_id, run.run_uid)
            .await?;
        assert_eq!(scores.score_run_id, run.score_run_id);
        assert!(scores.rows.is_empty());
        assert!(scores.trial_rollup_rows.is_empty());
        assert!(
            scores.trials.is_empty(),
            "no scorer has emitted analytics.scores rows yet"
        );

        assert_ne!(workflow.revision_uid, Uuid::nil());

        pool.close().await;
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn transaction_dispute_plan_clarifies_then_handles_required_review() -> Result<()> {
    // Pins: ambiguous transaction-dispute simulations ask clarifying questions before action review.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir
        .path()
        .join("transaction-dispute-behavior-lab-script.json");
    write_transaction_dispute_fixture(&fixture_path)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let workspace_id = WorkspaceId::new(tenant_id.to_string());
    let scope = ActionRuleScope::Tenant { tenant_id };
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let plan = import_and_publish_artifact(
            &client,
            ingress,
            &identity,
            &workspace_id,
            transaction_plan_source(),
        )
        .await?;

        let run = run_plan_experiment(
            &client,
            ingress,
            &identity,
            &workspace_id,
            "transaction-dispute-behavior-lab",
            plan.revision_uid,
        )
        .await?;
        assert_eq!(run.status, "accepted");
        assert_ne!(run.score_run_id, Uuid::nil());

        let status =
            wait_for_run_status(&client, ingress, &identity, &workspace_id, run.run_uid, |status| {
                status.status == "completed"
            })
            .await?;
        assert_eq!(status.status, "completed");
        assert_eq!(status.score_run_id, Some(run.score_run_id));

        let trials = list_trials(&client, ingress, &identity, &workspace_id, run.run_uid).await?;
        assert_eq!(trials.trials.len(), 1);
        let trial = &trials.trials[0];
        assert_eq!(trial.status, "completed");
        assert_eq!(trial.target_kind, "agent_loop");
        assert_eq!(trial.variant_key, "dispute-agent");
        assert_eq!(
            trial.scenario_id.as_deref(),
            Some("ambiguous-merchant-dispute")
        );
        assert_eq!(trial.turn_count, 2);
        assert_eq!(trial.stop_reason.as_deref(), Some("completed"));
        assert!(trial.workflow_run_uid.is_none());
        let session_id = trial
            .session_id
            .context("transaction-dispute trial should link target session")?;

        let trial_status = trial_status(
            &client,
            ingress,
            &identity,
            &workspace_id,
            trial.trial_uid,
        )
        .await?;
        assert_trial_status_matches_summary(&trial_status, trial);

        let events =
            wait_for_action_review_or_tool_result(&client, ingress, &identity, session_id).await?;
        assert_eq!(
            user_message_texts(&events),
            vec![
                "I see a card charge labeled SQ * CITY MARKET, but I do not know the exact merchant.",
                "It was $48.10 on May 8. I still do not recognize it and want to dispute it.",
            ]
        );
        assert_eq!(
            brain_response_texts(&events),
            vec![
                "Before drafting a dispute, please confirm the merchant's legal name, transaction date, amount, and whether your card was present.",
            ]
        );
        assert_eq!(tool_call_names(&events), vec!["bash".to_string()]);
        let action_reviews = action_review_request_count(&events);
        let successful_bash_results = successful_tool_results_for(&events, "bash");
        assert!(
            action_reviews == 1 || successful_bash_results == 1,
            "dispute action should either execute in auto mode or record an action review"
        );
        assert!(
            !events.iter().any(|record| matches!(
                &record.event,
                Event::ToolCall { tool_name, .. } if tool_name.starts_with("connector:")
                    || tool_name.starts_with("connector.")
            )),
            "target should not invent connector tools when the data bundle has only mock data"
        );

        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        assert_score_run_parent(&pool, &scope, run.score_run_id, "experiment_run").await?;
        assert_score_run_parent(&pool, &scope, trial.score_run_id, "experiment_trial").await?;
        assert_no_analytics_scores(&pool, &workspace_id, &[run.score_run_id, trial.score_run_id])
            .await?;
        assert_no_learning_candidates(&pool, &scope, &workspace_id).await?;
        pool.close().await;

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[test]
fn behavior_lab_simulation_path_does_not_auto_promote_or_fake_score_rows() {
    // Pins: simulation execution records score-run parents only; learning proposals stay explicit and require review.
    let run_workflow = include_str!("../src/workflows/experiment_run.rs");
    let trial_workflow = include_str!("../src/workflows/experiment_trial_run.rs");
    let experiments_service = include_str!("../src/services/experiments.rs");

    for source in [run_workflow, trial_workflow] {
        assert!(
            !source.contains("INSERT INTO analytics.scores"),
            "simulation workflows must not fake analytics.scores rows"
        );
        assert!(
            !source.contains("append_learning_candidate"),
            "simulation workflows must not auto-create learning candidates"
        );
        assert!(
            !source.contains("LearningCandidateStatus::Promoted"),
            "simulation workflows must not promote learned state"
        );
    }

    assert!(
        experiments_service.contains("append_learning_candidate(&candidate)"),
        "learning candidates should be created only by the explicit proposal operation"
    );
    assert!(
        experiments_service.contains("LearningCandidateStatus::Proposed"),
        "explicit proposals should wait for review"
    );
    assert!(
        !experiments_service.contains("LearningCandidateStatus::Promoted"),
        "experiment proposal service must not promote candidates"
    );
    assert!(
        !experiments_service.contains("publish_revision("),
        "experiment proposal service must not publish artifact changes"
    );
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_SIMULATION_TESTS=1 and live provider credentials"]
async fn live_behavior_lab_simulation_gate_requires_flag_and_provider_credentials() -> Result<()> {
    // Pins: live simulation tests are double-gated before any billed provider can be used.
    if std::env::var("MOA_RUN_LIVE_SIMULATION_TESTS").as_deref() != Ok("1") {
        return Ok(());
    }

    let has_credentials = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some());
    if !has_credentials {
        bail!(
            "MOA_RUN_LIVE_SIMULATION_TESTS=1 requires one of ANTHROPIC_API_KEY, OPENAI_API_KEY, or GOOGLE_API_KEY"
        );
    }

    Ok(())
}

async fn import_support_skill(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    let request = SkillImportRequest {
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::from(
                Uuid::parse_str(workspace_id.as_str()).context("workspace id is tenant uuid")?,
            ),
        },
        packages: vec![support_skill_package()],
    };
    let imported = post_json_with_identity(client, ingress, "Skills", "import", identity, &request)
        .await?
        .json::<SkillImportResponse>()
        .await
        .context("deserialize skill import response")?;
    assert_eq!(imported.imported, 1);
    Ok(())
}

async fn import_and_publish_artifact(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    source_text: &str,
) -> Result<ArtifactPublishResponse> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(
            Uuid::parse_str(workspace_id.as_str()).context("workspace id is tenant uuid")?,
        ),
    };
    let import_request = ArtifactImportRequest {
        scope,
        source_format: "yaml".to_string(),
        source_text: source_text.to_string(),
        files: Vec::new(),
    };
    let imported = post_json_with_identity(
        client,
        ingress,
        "Artifacts",
        "import",
        identity,
        &import_request,
    )
    .await?
    .json::<ArtifactImportResponse>()
    .await
    .context("deserialize artifact import response")?;
    assert_eq!(imported.status, "draft");

    let publish_request = ArtifactPublishRequest {
        scope,
        revision_uid: imported.revision_uid,
    };
    let published = post_json_with_identity(
        client,
        ingress,
        "Artifacts",
        "publish",
        identity,
        &publish_request,
    )
    .await?
    .json::<ArtifactPublishResponse>()
    .await
    .context("deserialize artifact publish response")?;
    assert_eq!(published.status, "published");
    assert_validation_report_has_no_errors(&published.validation_report)?;
    Ok(published)
}

async fn run_plan_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    name: &str,
    plan_revision_uid: Uuid,
) -> Result<ExperimentRunResponse> {
    let request = ExperimentRunRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        name: name.to_string(),
        plan_revision_uid: Some(plan_revision_uid),
        target: None,
        variant: None,
        scorecard: json!({}),
        score_run_id: None,
        idempotency_key: Some(format!("{name}-{}", Uuid::now_v7())),
        agent_revision_variants: Vec::new(),
    };
    post_json_with_identity(client, ingress, "Experiments", "run", identity, &request)
        .await?
        .json::<ExperimentRunResponse>()
        .await
        .context("deserialize experiment run response")
}

async fn wait_for_run_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_uid: Uuid,
    done: impl Fn(&ExperimentRunStatusResponse) -> bool,
) -> Result<ExperimentRunStatusResponse> {
    let request = ExperimentRunStatusRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        run_uid,
    };
    let mut last_status = None;
    for _attempt in 0..90 {
        let status =
            post_json_with_identity(client, ingress, "Experiments", "status", identity, &request)
                .await?
                .json::<ExperimentRunStatusResponse>()
                .await
                .context("deserialize experiment status response")?;
        if done(&status) {
            return Ok(status);
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for experiment {run_uid}; last status: {last_status:?}")
}

async fn list_trials(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_uid: Uuid,
) -> Result<ExperimentTrialsResponse> {
    let request = ExperimentTrialsRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        run_uid,
        status: None,
        limit: Some(10),
    };
    post_json_with_identity(client, ingress, "Experiments", "trials", identity, &request)
        .await?
        .json::<ExperimentTrialsResponse>()
        .await
        .context("deserialize experiment trials response")
}

async fn trial_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    trial_uid: Uuid,
) -> Result<ExperimentTrialStatusResponse> {
    let request = ExperimentTrialStatusRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        trial_uid,
    };
    post_json_with_identity(
        client,
        ingress,
        "Experiments",
        "trial_status",
        identity,
        &request,
    )
    .await?
    .json::<ExperimentTrialStatusResponse>()
    .await
    .context("deserialize experiment trial status response")
}

async fn experiment_scores(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_uid: Uuid,
) -> Result<ExperimentScoresResponse> {
    let request = ExperimentScoresRequest {
        tenant_id: tenant_id_from_workspace(workspace_id)?,
        run_uid,
    };
    post_json_with_identity(client, ingress, "Experiments", "scores", identity, &request)
        .await?
        .json::<ExperimentScoresResponse>()
        .await
        .context("deserialize experiment scores response")
}

async fn run_workflow_for_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    session_id: SessionId,
) -> Result<WorkflowRunResponse> {
    let request = WorkflowRunRequest {
        tenant_id: TenantId::from(
            Uuid::parse_str(workspace_id.as_str()).context("workspace id is tenant uuid")?,
        ),
        workflow_ref: "workflow://damaged-food-replacement".to_string(),
        input: json!({
            "order_id": "FOOD-42",
            "damage_summary": "soup pooled under the container and sauce on every item",
            "customer_requested": "replacement"
        }),
        session_id: Some(session_id),
        idempotency_key: Some(format!("damaged-food-workflow-{}", Uuid::now_v7())),
    };
    post_json_with_identity(client, ingress, "Workflows", "run", identity, &request)
        .await?
        .json::<WorkflowRunResponse>()
        .await
        .context("deserialize workflow run response")
}

async fn workflow_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_id: Uuid,
) -> Result<WorkflowRunStatus> {
    let request = WorkflowStatusRequest {
        tenant_id: TenantId::from(
            Uuid::parse_str(workspace_id.as_str()).context("workspace id is tenant uuid")?,
        ),
        run_id,
    };
    post_json_with_identity(client, ingress, "Workflows", "status", identity, &request)
        .await?
        .json::<WorkflowRunStatus>()
        .await
        .context("deserialize workflow status response")
}

async fn wait_for_session_messages(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    expected_user_messages: usize,
    expected_brain_responses: usize,
) -> Result<Vec<EventRecord>> {
    let mut last_events = Vec::new();
    for _attempt in 0..90 {
        let events = fetch_events(client, ingress, identity, session_id).await?;
        if user_message_texts(&events).len() == expected_user_messages
            && brain_response_texts(&events).len() == expected_brain_responses
        {
            return Ok(events);
        }
        last_events = events;
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for session {session_id} messages; observed events: {}",
        summarize_events(&last_events)
    )
}

async fn wait_for_action_review_or_tool_result(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    let mut last_events = Vec::new();
    for _attempt in 0..90 {
        let events = fetch_events(client, ingress, identity, session_id).await?;
        if action_review_request_count(&events) == 1
            || successful_tool_results_for(&events, "bash") == 1
        {
            return Ok(events);
        }
        last_events = events;
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for action review or tool result in session {session_id}; observed events: {}",
        summarize_events(&last_events)
    )
}

async fn fetch_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    post_json_with_identity(
        client,
        ingress,
        "SessionStore",
        "get_events",
        identity,
        &get_events_request(session_id, EventRange::all()),
    )
    .await?
    .json::<Vec<EventRecord>>()
    .await
    .context("deserialize session events")
}

async fn post_json_with_identity<T: serde::Serialize + ?Sized>(
    client: &reqwest::Client,
    ingress: &str,
    service: &str,
    handler: &str,
    identity: &Identity,
    request: &T,
) -> Result<reqwest::Response> {
    let response = with_identity(
        client.post(service_url(ingress, service, handler)),
        identity,
    )
    .json(request)
    .send()
    .await
    .with_context(|| format!("call {service}/{handler}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    bail!("{service}/{handler} returned {status}: {body}")
}

fn service_url(ingress: &str, service: &str, handler: &str) -> String {
    format!("{}/{service}/{handler}", ingress.trim_end_matches('/'))
}

async fn assert_score_run_parent(
    pool: &PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    source: &str,
) -> Result<()> {
    let (scope_label, workspace_id, user_id) = scope_parts(scope);
    let scope_context = scope_context(scope);
    let mut conn = ScopedConn::begin(pool, &scope_context).await?;
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM analytics.score_run
            WHERE run_id = $1
              AND source = $5
              AND scope = $2
              AND workspace_id IS NOT DISTINCT FROM $3
              AND user_id IS NOT DISTINCT FROM $4
        )
        "#,
    )
    .bind(score_run_id)
    .bind(scope_label)
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .bind(source)
    .fetch_one(conn.as_mut())
    .await
    .with_context(|| format!("query score_run parent {score_run_id}"))?;
    conn.commit().await?;
    assert!(
        exists,
        "expected score_run parent {score_run_id} with source {source}"
    );
    Ok(())
}

async fn assert_no_analytics_scores(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    score_run_ids: &[Uuid],
) -> Result<()> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM analytics.scores
        WHERE workspace_id = $1
          AND run_id = ANY($2)
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(score_run_ids)
    .fetch_one(pool)
    .await
    .context("count behavior-lab analytics score rows")?;
    assert_eq!(
        count, 0,
        "simulation should not fake analytics.scores rows before a scorer emits them"
    );
    Ok(())
}

async fn assert_no_learning_candidates(
    pool: &PgPool,
    scope: &ActionRuleScope,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    let scope_context = scope_context(scope);
    let mut conn = ScopedConn::begin(pool, &scope_context).await?;
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM learning_candidates
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .context("count learning candidates for behavior-lab workspace")?;
    conn.commit().await?;
    assert_eq!(
        count, 0,
        "simulation should not auto-promote or propose learning"
    );
    Ok(())
}

fn scope_parts(scope: &ActionRuleScope) -> (&'static str, Option<String>, Option<String>) {
    match scope {
        ActionRuleScope::WorkspaceDefault => ("global", None, None),
        ActionRuleScope::Tenant { tenant_id } => ("workspace", Some(tenant_id.to_string()), None),
    }
}

fn scope_context(scope: &ActionRuleScope) -> ScopeContext {
    match scope {
        ActionRuleScope::WorkspaceDefault => ScopeContext::tenant(TenantId::from(Uuid::nil())),
        ActionRuleScope::Tenant { tenant_id } => ScopeContext::tenant(*tenant_id),
    }
}

fn tenant_id_from_workspace(workspace_id: &WorkspaceId) -> Result<TenantId> {
    Uuid::parse_str(workspace_id.as_str())
        .map(TenantId::from)
        .context("workspace fixture id should be a tenant UUID")
}

fn assert_trial_status_matches_summary(
    status: &ExperimentTrialStatusResponse,
    summary: &ExperimentTrialSummary,
) {
    assert_eq!(status.tenant_id, summary.tenant_id);
    assert_eq!(status.run_uid, summary.run_uid);
    assert_eq!(status.trial_uid, summary.trial_uid);
    assert_eq!(status.status, summary.status);
    assert_eq!(status.target_kind, summary.target_kind);
    assert_eq!(status.trial_key, summary.trial_key);
    assert_eq!(status.variant_key, summary.variant_key);
    assert_eq!(status.scenario_id, summary.scenario_id);
    assert_eq!(status.score_run_id, summary.score_run_id);
    assert_eq!(status.session_id, summary.session_id);
    assert_eq!(status.workflow_run_uid, summary.workflow_run_uid);
    assert_eq!(status.stop_reason, summary.stop_reason);
    assert_eq!(status.turn_count, summary.turn_count);
}

fn user_message_texts(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn brain_response_texts(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_call_names(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

fn action_review_request_count(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::ActionReviewRequested { .. }))
        .count()
}

fn successful_tool_results_for(events: &[EventRecord], tool_name: &str) -> usize {
    let tool_ids = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name: event_tool_name,
                ..
            } if event_tool_name == tool_name => Some(*tool_id),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();

    events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ToolResult {
                    tool_id,
                    success: true,
                    ..
                } if tool_ids.contains(tool_id)
            )
        })
        .count()
}

fn saw_successful_skill_file_read(events: &[EventRecord]) -> bool {
    let read_tool_ids = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } if tool_name == "file_read"
                && input.get("path").and_then(Value::as_str) == Some(SUPPORT_SKILL_PATH) =>
            {
                Some(*tool_id)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();

    events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolResult {
                tool_id,
                output,
                success: true,
                ..
            } if read_tool_ids.contains(tool_id) && output.to_text().contains("Delivery Support")
        )
    })
}

fn summarize_events(events: &[EventRecord]) -> String {
    if events.is_empty() {
        return "<none>".to_string();
    }

    events
        .iter()
        .map(|record| format!("#{} {:?}", record.sequence_num, record.event_type))
        .collect::<Vec<_>>()
        .join(", ")
}

fn assert_validation_report_has_no_errors(report: &Value) -> Result<()> {
    let Some(errors) = report.get("errors").and_then(Value::as_array) else {
        bail!("validation report did not include an errors array: {report}");
    };
    if errors.is_empty() {
        return Ok(());
    }

    bail!("published artifact had validation errors: {errors:?}")
}

fn write_damaged_food_fixture(path: &Path) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "responses": [
            {
                "completion": {
                    "content": "The photo is blurry, but it looks like soup leaked through the delivery bag.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "file_read",
                        "id": SUPPORT_SKILL_PROVIDER_ID,
                        "input": { "path": SUPPORT_SKILL_PATH }
                    }]
                }
            },
            {
                "completion": {
                    "content": "I can help, but the photo is unclear. Please describe the damage and share the order id before I recommend a replacement.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "Order FOOD-42 arrived with soup pooled under the container and sauce on every item.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "Thanks for the clearer description. The damaged-food workflow can be associated with FOOD-42 for replacement review.",
                    "tool_calls": []
                }
            }
        ]
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize damaged-food fixture")?;
    fs::write(path, body).context("write damaged-food fixture")
}

fn write_transaction_dispute_fixture(path: &Path) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "responses": [
            {
                "completion": {
                    "content": "I see a card charge labeled SQ * CITY MARKET, but I do not know the exact merchant.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "Before drafting a dispute, please confirm the merchant's legal name, transaction date, amount, and whether your card was present.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "It was $48.10 on May 8. I still do not recognize it and want to dispute it.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "bash",
                        "id": "draft-dispute-review-tool-call",
                        "input": { "cmd": "printf 'draft transaction dispute for review\\n'" }
                    }]
                }
            }
        ]
    });
    let body =
        serde_json::to_vec_pretty(&fixture).context("serialize transaction-dispute fixture")?;
    fs::write(path, body).context("write transaction-dispute fixture")
}

fn support_skill_package() -> SkillPackageDocument {
    let skill_md = r#"---
name: delivery-support
description: "Resolve damaged or spilled food delivery support requests from clear customer evidence."
allowed-tools: file_read
metadata:
  moa-tags: "support,delivery,refund,replacement,food"
  moa-use-count: "6"
  moa-success-rate: "0.96"
---

# Delivery Support

Use this when a customer reports a delivery that arrived spilled, crushed, leaking, missing items, unsafe, or otherwise damaged.

When there is a clear customer description and order id, recommend a replacement review or refund review. Ask for clearer evidence when the photo or description is ambiguous.
"#;
    SkillPackageDocument {
        name: Some("delivery-support".to_string()),
        description: Some(
            "Resolve damaged or spilled food delivery support requests from clear customer evidence."
                .to_string(),
        ),
        files: vec![SkillPackageDocumentFile {
            path: "SKILL.md".to_string(),
            content_base64: BASE64.encode(skill_md.as_bytes()),
            content_type: Some("text/markdown".to_string()),
            executable: false,
        }],
        source_uri: Some("test://behavior-lab/delivery-support".to_string()),
        metadata: json!({}),
    }
}

fn damaged_food_plan_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: experiment_plan
metadata:
  name: damaged-food-behavior-lab
  description: Behavior-lab plan for damaged food support.
status: draft
definition:
  type: experiment_plan
  spec:
    simulation:
      scenarios:
        - id: damaged-food-unclear-photo
          initial_situation: The customer reports damaged food with an unclear photo.
          goals:
            - Agent asks for clearer evidence when the photo is ambiguous.
            - Agent uses support instructions before recommending a next step.
          allowed_user_intents:
            - report_damaged_food
            - clarify_damage
          success_criteria:
            - The target asks for clearer details before replacement review.
            - The target links the case to the damaged-food workflow after clarification.
          failure_criteria:
            - The target promises a refund before evidence is clear.
          max_turns: 2
          admin_review_behavior: stop_on_admin_review
          scoring_rubric:
            score_names:
              - evidence_clarified
      personas:
        - id: damaged-food-unclear-photo
          voice: Patient, concise, and mildly frustrated.
          goals:
            - Get a replacement or refund review for a damaged delivery.
          constraints:
            - Do not invent a clear photo when the first message says the photo is blurry.
            - Provide a clearer text description only after the agent asks.
          likely_missing_information:
            - order id
            - clear damage description
          stop_behavior: Stop after the agent gives a concrete replacement-review next step.
      profiles:
        - id: damaged-food-order-profile
          facts:
            order_id: FOOD-42
            merchant: Noodle House
            item: tomato soup combo
            delivery_state: arrived damaged
          data_classification: synthetic_test_data
    target_variants:
      - key: support-agent
        kind: agent_loop
        workflow_ref: workflow://damaged-food-replacement
        config:
          prompt: Start the damaged-food support trial. Use the delivery support skill before recommending a refund or replacement, and associate the damaged-food workflow once details are clear.
    simulator_model: scripted-loadtest
    target_model: scripted-loadtest
    parallelism: 1
    trials_per_combination: 1
    budget:
      max_total_cents: 100
      max_trial_cents: 100
      max_total_tokens: 10000
      max_trial_tokens: 2000
    scorecard:
      metrics:
        - evidence_clarified
    learning_proposals:
      enabled: false
"#
}

fn damaged_food_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: damaged-food-replacement
  description: Procedure for replacement review when food arrives damaged.
  tags:
    - support
    - food-delivery
    - replacement
status: draft
definition:
  type: workflow
  spec:
    input_schema:
      type: object
      required:
        - order_id
        - damage_summary
      properties:
        order_id:
          type: string
        damage_summary:
          type: string
        customer_requested:
          type: string
    state_schema:
      type: object
      properties:
        evidence_sufficient:
          type: boolean
    nodes:
      - id: start
        kind: start
      - id: verify_evidence
        kind: condition
        condition:
          type: exists
          path: $.damage_summary
      - id: choose_resolution
        kind: agent
        max_turns: 2
        input:
          instruction: Decide replacement, refund review, or escalation after evidence review.
      - id: done
        kind: end
    edges:
      - id: start-to-verify
        from: start
        to: verify_evidence
      - id: verify-to-resolution
        from: verify_evidence
        to: choose_resolution
      - id: resolution-to-done
        from: choose_resolution
        to: done
"#
}

fn transaction_plan_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: experiment_plan
metadata:
  name: transaction-dispute-behavior-lab
  description: Behavior-lab plan for transaction dispute clarification.
status: draft
definition:
  type: experiment_plan
  spec:
    simulation:
      scenarios:
        - id: ambiguous-merchant-dispute
          initial_situation: The user sees an unfamiliar card charge with an ambiguous Square merchant label.
          goals:
            - The target asks for clarifying transaction details before drafting a dispute.
            - The target reaches action review before taking the dispute action when review policy applies.
          allowed_user_intents:
            - ask_about_dispute
            - provide_partial_transaction_details
          success_criteria:
            - Clarifying question asks for merchant, date, amount, and authorization context.
            - Action review is recorded before the dispute action executes when review policy applies.
          failure_criteria:
            - The target files or drafts a dispute action without required review.
          max_turns: 3
          admin_review_behavior: stop_on_admin_review
          data_bundle_ids:
            - transaction-dispute-mock-data
          scoring_rubric:
            score_names:
              - clarifies_before_dispute
      personas:
        - id: ambiguous-dispute-cardholder
          voice: Concerned and uncertain.
          goals:
            - Understand whether an unfamiliar card charge can be disputed.
          constraints:
            - Begin with ambiguous merchant details.
            - Provide amount and date only after the agent asks.
          likely_missing_information:
            - legal merchant name
            - card-present status
          stop_behavior: Stop when the target reaches action review or asks for the required dispute details.
      profiles:
        - id: ambiguous-dispute-profile
          facts:
            posted_label: SQ * CITY MARKET
            amount: 48.10
            posted_date: "2026-05-08"
            recognized: false
          data_classification: synthetic_test_data
      data_bundles:
        - id: transaction-dispute-mock-data
          sources:
            - id: posted-card-charge
              kind: mock_data
              fixture:
                posted_label: SQ * CITY MARKET
                amount: 48.10
                posted_date: "2026-05-08"
              notes: Inline mock data only; no connector_ref is available.
    target_variants:
      - key: dispute-agent
        kind: agent_loop
        config:
          prompt: Ask clarifying questions for ambiguous merchant details. Do not draft a dispute action until the required details are present and action review is satisfied when required.
    simulator_model: scripted-loadtest
    target_model: scripted-loadtest
    parallelism: 1
    trials_per_combination: 1
    budget:
      max_total_cents: 100
      max_trial_cents: 100
      max_total_tokens: 10000
      max_trial_tokens: 2000
    scorecard:
      metrics:
        - clarifies_before_dispute
    learning_proposals:
      enabled: false
"#
}
