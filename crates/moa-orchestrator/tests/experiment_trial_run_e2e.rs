//! End-to-end coverage for behavior-lab trial execution through Restate.

use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_artifacts::validation::validate_for_status;
use moa_core::{
    ActionRuleScope, Event, EventRange, EventRecord, ModelId, SessionId, SessionMeta, TenantId,
    traits::Identity,
    wire::procedures::{
        ProcedureReviewDecisionKind, ProcedureReviewDecisionRequest,
        ProcedureReviewDecisionResponse, ProcedureRunStatus, ProcedureStatusRequest,
    },
    wire::turn::{TurnOutcome, TurnOutcomeKind},
};
use moa_experiments::{
    model::{
        ExperimentScorecard, ExperimentSimulatorConfig, ExperimentTrialRecord, NewExperimentRun,
        NewExperimentTrial,
    },
    store::ExperimentStore,
};
use moa_orchestrator::objects::session::{AttachSessionTurnWaiterInput, SessionClient};
use moa_orchestrator::workflows::experiment_trial_run::{
    ExperimentTrialRunStatusRequest, ExperimentTrialRunStatusResponse,
    ExperimentTrialRunWorkflowRequest, trial_workflow_key,
};
use moa_test_support::postgres::test_database_url;
use restate_sdk::prelude::{
    ContextAwakeables, ContextClient, ContextReadState, ContextWriteState, Endpoint, HandlerResult,
    HttpServer, Json, SharedWorkflowContext, TerminalError, WorkflowContext,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::support::{
    restate_runtime::{
        OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_tenant_admin,
        register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
        test_user_identity, with_identity,
    },
    session_store_service::get_events_request,
};

#[path = "support/mod.rs"]
mod support;

const K_PROBE_ATTACHED: &str = "attached";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionTurnWaiterProbeInput {
    session_id: SessionId,
    turn_id: String,
}

#[restate_sdk::workflow]
trait SessionTurnWaiterProbe {
    async fn run(input: Json<SessionTurnWaiterProbeInput>) -> HandlerResult<Json<TurnOutcome>>;

    #[shared]
    async fn attached() -> HandlerResult<Json<bool>>;
}

struct SessionTurnWaiterProbeImpl;

impl SessionTurnWaiterProbe for SessionTurnWaiterProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        input: Json<SessionTurnWaiterProbeInput>,
    ) -> HandlerResult<Json<TurnOutcome>> {
        let input = input.into_inner();
        let (awakeable_id, completion) = ctx.awakeable::<String>();
        let attached = ctx
            .object_client::<SessionClient>(input.session_id.to_string())
            .attach_turn_waiter(Json::from(AttachSessionTurnWaiterInput {
                turn_id: input.turn_id,
                awakeable_id,
            }))
            .call()
            .await?
            .into_inner();
        ctx.set(K_PROBE_ATTACHED, Json::from(true));

        if let Some(outcome) = attached.outcome {
            return Ok(Json::from(outcome));
        }

        let payload = completion.await?;
        let outcome = serde_json::from_str::<TurnOutcome>(&payload).map_err(|error| {
            TerminalError::new(format!("deserialize session turn waiter outcome: {error}"))
        })?;
        Ok(Json::from(outcome))
    }

    async fn attached(&self, ctx: SharedWorkflowContext<'_>) -> HandlerResult<Json<bool>> {
        let attached = ctx
            .get::<Json<bool>>(K_PROBE_ATTACHED)
            .await?
            .map(Json::into_inner)
            .unwrap_or(false);
        Ok(Json::from(attached))
    }
}

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
        .env_remove("MOA_ANTHROPIC_API_KEY")
        .env_remove("MOA_OPENAI_API_KEY")
        .env_remove("MOA_GOOGLE_API_KEY")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for trial e2e")
}

fn spawn_orchestrator_without_provider_override(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    database_url: &str,
) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", database_url)
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env("MOA_SKIP_FGA", "true")
        .env_remove("MOA_PROVIDERS_OVERRIDE")
        .env_remove("MOA_ANTHROPIC_API_KEY")
        .env_remove("MOA_OPENAI_API_KEY")
        .env_remove("MOA_GOOGLE_API_KEY")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for session waiter e2e")
}

/// Spawns an orchestrator against the shared test database with real OpenFGA and
/// no scripted provider override. Procedure trials run deterministic procedures
/// and never call a provider, so no scripted fixture is required.
fn spawn_orchestrator_no_provider_override_with_fga(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
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
        .env("RUST_LOG", "info")
        .env_remove("MOA_PROVIDERS_OVERRIDE")
        .env_remove("MOA_ANTHROPIC_API_KEY")
        .env_remove("MOA_OPENAI_API_KEY")
        .env_remove("MOA_GOOGLE_API_KEY")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for procedure trial e2e")
}

struct ProbeEndpoint {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl ProbeEndpoint {
    async fn stop(self) {
        self.cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handle).await;
    }
}

async fn spawn_session_turn_waiter_probe(port: u16) -> Result<ProbeEndpoint> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind session turn waiter probe endpoint on port {port}"))?;
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let handle = tokio::spawn(async move {
        HttpServer::new(
            Endpoint::builder()
                .bind(SessionTurnWaiterProbeImpl.serve())
                .build(),
        )
        .serve_with_cancel(listener, shutdown.cancelled_owned())
        .await;
    });
    Ok(ProbeEndpoint { cancel, handle })
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn experiment_trial_run_drives_multiturn_scripted_agent_loop() -> Result<()> {
    // Pins: ExperimentTrialRun itself can drive a deterministic multi-turn simulator trial.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("experiment-trial-run-script.json");
    write_scripted_fixture(&fixture_path)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let scope = ActionRuleScope::Tenant { tenant_id };
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        let store = ExperimentStore::new(pool.clone());
        let agent_revision_uid = publish_trial_agent(&pool, &scope).await?;
        let run = store
            .insert_run(&scope, new_parent_run(&identity, agent_revision_uid))
            .await
            .context("seed parent experiment run")?;
        let plan_revision_uid = publish_trial_plan(&pool, &scope, agent_revision_uid).await?;
        let trial = new_trial(run.run_uid, plan_revision_uid);
        let trial_key = trial.trial_key.clone();
        let workflow_request = ExperimentTrialRunWorkflowRequest {
            tenant_id,
            trial: trial.clone(),
            target: agent_loop_target(agent_revision_uid),
            variant: baseline_variant(),
            identity: identity.clone(),
            completion_awakeable_id: None,
        };

        let first = run_trial_workflow(
            &client,
            ingress,
            &identity,
            run.run_uid,
            &trial_key,
            &workflow_request,
        )
        .await?;

        assert_eq!(first.status, "completed");
        assert_eq!(first.stop_reason.as_deref(), Some("max_turns"));
        assert_eq!(first.turn_count, 2);
        assert_eq!(first.run_uid, run.run_uid);
        assert_ne!(first.trial_uid, Uuid::nil());
        let session_id = first
            .session_id
            .context("trial workflow should expose the linked target session")?;

        let persisted = store
            .load_trial(&scope, first.trial_uid)
            .await
            .context("load persisted trial")?
            .context("persisted trial should exist")?;
        assert_persisted_trial_matches_response(&persisted, &first, session_id);

        let events = wait_for_target_messages(&client, ingress, &identity, session_id).await?;
        assert_eq!(
            user_message_texts(&events),
            vec![
                "Hi, I need help with a delayed order.",
                "The order id is ORDER-42. Can you check it?",
            ]
        );
        assert_eq!(
            brain_response_texts(&events),
            vec![
                "I can help with the delayed order. What is your order id?",
                "Thanks, I checked ORDER-42 and the delivery support case is ready for follow-up.",
            ]
        );
        assert!(
            !events
                .iter()
                .any(|record| matches!(record.event, Event::ToolCall { .. })),
            "simulator trial fixture should not expose or exercise target tools"
        );

        let retry = read_trial_workflow_status(
            &client,
            ingress,
            &identity,
            run.run_uid,
            &trial_key,
            tenant_id,
            first.trial_uid,
        )
        .await?;
        assert_eq!(retry.trial_uid, first.trial_uid);
        assert_eq!(retry.session_id, first.session_id);
        assert_eq!(retry.score_run_id, first.score_run_id);
        assert_eq!(retry.turn_count, first.turn_count);
        let trials = store
            .list_trials(&scope, run.run_uid, None, 10)
            .await
            .context("list persisted trials")?;
        assert_eq!(
            trials.len(),
            1,
            "retry with the same key must be idempotent"
        );

        pool.close().await;
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and OpenFGA"]
async fn procedure_trial_awaits_pending_review_procedure_before_resolving_service_e2e() -> Result<()>
{
    // Pins: a procedure-backed trial does not resolve while its procedure is still executing.
    // The trial's ExperimentTrialRun/run invocation must stay in-flight while the procedure is
    // paused on a review node, and only return a terminal (completed) status once the review is
    // decided and the procedure reaches a terminal state. The prior fire-and-forget `.send()`
    // returned a non-terminal "running" response immediately, resolving the parent fan-in while
    // the procedure was still running.
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
    let scope = ActionRuleScope::Tenant { tenant_id };
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator =
        spawn_orchestrator_no_provider_override_with_fga(ports, &memory_dir, &sandbox_dir)?;

    let result = async {
        wait_for_orchestrator_live(&client, ports.health).await?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        let store = ExperimentStore::new(pool.clone());

        let agent_revision_uid = publish_trial_agent(&pool, &scope).await?;
        let plan_revision_uid = publish_trial_plan(&pool, &scope, agent_revision_uid).await?;
        publish_artifact_revision(
            &pool,
            &scope,
            review_gated_procedure_source(),
            "review-gated procedure",
        )
        .await?;

        let run = store
            .insert_run(&scope, new_parent_run(&identity, agent_revision_uid))
            .await
            .context("seed parent experiment run")?;
        let trial = new_procedure_trial(run.run_uid, plan_revision_uid);
        let trial_key = trial.trial_key.clone();
        let workflow_request = ExperimentTrialRunWorkflowRequest {
            tenant_id,
            trial: trial.clone(),
            target: procedure_review_target(),
            variant: baseline_variant(),
            identity: identity.clone(),
            completion_awakeable_id: None,
        };

        // Invoke ExperimentTrialRun/run in the background: with the fix it blocks until the
        // procedure reaches a terminal state, so the request stays in-flight until the review
        // is decided below.
        let mut run_task = spawn_procedure_trial_run(
            &client,
            ingress,
            &identity,
            run.run_uid,
            &trial_key,
            &workflow_request,
        );

        // Wait until the trial has started and linked its procedure run.
        let linked = wait_for_trial_procedure_link(&store, &scope, run.run_uid, &trial_key).await?;
        let procedure_run_uid = linked
            .procedure_run_uid
            .context("procedure trial should link a procedure run")?;

        // Wait until the procedure has paused on its review node.
        let pending = wait_for_procedure_status(
            &client,
            ingress,
            &identity,
            tenant_id,
            procedure_run_uid,
            "pending_review",
        )
        .await?;
        assert_eq!(pending.current_node_id.as_deref(), Some("gate"));

        // While the procedure is paused, the trial must not have resolved: the persisted trial
        // is still running and the run invocation is still in-flight.
        let blocked = store
            .load_trial_by_key(&scope, run.run_uid, &trial_key)
            .await
            .context("load trial while procedure is paused")?
            .context("trial should exist while procedure is paused")?;
        assert_eq!(
            blocked.status.as_str(),
            "running",
            "trial must remain running while its procedure is paused on review"
        );
        assert!(
            !run_task.is_finished(),
            "ExperimentTrialRun/run must stay in-flight while the procedure is paused on review"
        );

        // Decide the review; the procedure resumes to completion and the trial can resolve.
        let decision =
            decide_procedure_review(&client, ingress, &identity, tenant_id, procedure_run_uid)
                .await?;
        assert!(decision.accepted);

        let response = tokio::time::timeout(Duration::from_secs(60), &mut run_task)
            .await
            .context("timed out waiting for ExperimentTrialRun/run to resolve after review")?
            .context("join ExperimentTrialRun/run task")??;
        assert_eq!(
            response.status, "completed",
            "trial should report a terminal completed status once the procedure completes"
        );
        assert_eq!(response.stop_reason.as_deref(), Some("target_terminal"));
        assert_eq!(response.procedure_run_uid, Some(procedure_run_uid));

        let persisted = store
            .load_trial_by_key(&scope, run.run_uid, &trial_key)
            .await
            .context("load persisted trial after completion")?
            .context("trial should exist after completion")?;
        assert_eq!(persisted.status.as_str(), "completed");

        pool.close().await;
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server and Postgres"]
async fn session_turn_waiter_resolves_recorded_outcome_through_restate_service() -> Result<()> {
    // Pins: Session attach_turn_waiter -> record_turn_outcome resolves registered awakeables.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let probe_ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let probe_endpoint_url = deployment_endpoint_url(probe_ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let database = IsolatedDatabase::create("session_waiter").await?;
    let probe_endpoint = match spawn_session_turn_waiter_probe(probe_ports.restate).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            database.drop_database().await?;
            return Err(error);
        }
    };
    let mut orchestrator = match spawn_orchestrator_without_provider_override(
        ports,
        &memory_dir,
        &sandbox_dir,
        &database.database_url,
    ) {
        Ok(child) => child,
        Err(error) => {
            probe_endpoint.stop().await;
            database.drop_database().await?;
            return Err(error);
        }
    };

    let result = async {
        wait_for_orchestrator_live(&client, ports.health).await?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        register_deployment(&restate_admin_url(), probe_endpoint_url.as_str()).await?;
        let session_id = SessionId::new();
        let meta = SessionMeta {
            id: session_id,
            tenant_id: TenantId::new(),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        };
        post_json(
            &client,
            ingress,
            &format!("Session/{session_id}"),
            "set_meta",
            &meta,
        )
        .await?;

        let turn_id = format!("service-waiter-turn-{}", Uuid::now_v7());
        let probe_key = format!("session-waiter-probe-{}", Uuid::now_v7());
        let mut probe_task =
            spawn_session_turn_waiter_probe_run(&client, ingress, &probe_key, session_id, &turn_id);
        if let Err(error) =
            wait_for_session_turn_waiter_probe_attached(&client, ingress, &probe_key).await
        {
            probe_task.abort();
            return Err(error);
        }

        let outcome = TurnOutcome {
            turn_id: turn_id.clone(),
            kind: TurnOutcomeKind::Completed,
            message: "service-level waiter completed".to_string(),
        };
        post_json(
            &client,
            ingress,
            &format!("Session/{session_id}"),
            "record_turn_outcome",
            &outcome,
        )
        .await?;

        let resolved = match wait_for_session_turn_waiter_probe_outcome(&mut probe_task).await {
            Ok(outcome) => outcome,
            Err(error) => {
                probe_task.abort();
                return Err(error);
            }
        };
        assert_eq!(resolved, outcome);
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    probe_endpoint.stop().await;
    database.drop_database().await?;

    result
}

async fn run_trial_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    run_uid: Uuid,
    trial_key: &str,
    request: &ExperimentTrialRunWorkflowRequest,
) -> Result<ExperimentTrialRunStatusResponse> {
    let key = trial_workflow_key(run_uid, trial_key);
    post_json_with_identity(
        client,
        ingress,
        &format!("ExperimentTrialRun/{key}"),
        "run",
        identity,
        request,
    )
    .await?
    .json::<ExperimentTrialRunStatusResponse>()
    .await
    .context("deserialize ExperimentTrialRun/run response")
}

async fn read_trial_workflow_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    run_uid: Uuid,
    trial_key: &str,
    tenant_id: TenantId,
    trial_uid: Uuid,
) -> Result<ExperimentTrialRunStatusResponse> {
    let key = trial_workflow_key(run_uid, trial_key);
    let request = ExperimentTrialRunStatusRequest {
        tenant_id,
        trial_uid,
    };
    post_json_with_identity(
        client,
        ingress,
        &format!("ExperimentTrialRun/{key}"),
        "status",
        identity,
        &request,
    )
    .await?
    .json::<ExperimentTrialRunStatusResponse>()
    .await
    .context("deserialize ExperimentTrialRun/status response")
}

async fn wait_for_target_messages(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    let mut last_events = Vec::new();
    for _attempt in 0..60 {
        let events = fetch_events(client, ingress, identity, session_id).await?;
        if user_message_texts(&events).len() == 2 && brain_response_texts(&events).len() == 2 {
            return Ok(events);
        }
        last_events = events;
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for two target messages and responses in session {session_id}; observed events: {}",
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

async fn wait_for_orchestrator_live(client: &reqwest::Client, health_port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{health_port}/_health/live");
    let mut last_observation = "not yet checked".to_string();
    for _attempt in 0..120 {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_observation = format!("health returned {}", response.status());
            }
            Err(error) => {
                last_observation = error.to_string();
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!("timed out waiting for spawned orchestrator health at {url}: {last_observation}")
}

struct IsolatedDatabase {
    name: String,
    maintenance_url: String,
    database_url: String,
}

impl IsolatedDatabase {
    async fn create(label: &str) -> Result<Self> {
        let base_url = test_database_url();
        let name = format!("moa_task7_{label}_{}", Uuid::now_v7().simple());
        let maintenance_url = replace_database_name(&base_url, "postgres")?;
        let database_url = replace_database_name(&base_url, &name)?;
        let pool = PgPool::connect(&maintenance_url)
            .await
            .context("connect to Postgres maintenance database")?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&pool)
            .await
            .with_context(|| format!("create isolated database {name}"))?;
        pool.close().await;
        Ok(Self {
            name,
            maintenance_url,
            database_url,
        })
    }

    async fn drop_database(&self) -> Result<()> {
        let pool = PgPool::connect(&self.maintenance_url)
            .await
            .context("connect to Postgres maintenance database for cleanup")?;
        sqlx::query(
            "SELECT pg_terminate_backend(pid) \
             FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(&self.name)
        .execute(&pool)
        .await
        .with_context(|| format!("terminate isolated database connections for {}", self.name))?;
        sqlx::query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute(&pool)
            .await
            .with_context(|| format!("drop isolated database {}", self.name))?;
        pool.close().await;
        Ok(())
    }
}

fn replace_database_name(database_url: &str, database_name: &str) -> Result<String> {
    let (prefix, database_and_query) = database_url
        .rsplit_once('/')
        .context("database URL should include a database path")?;
    let query = database_and_query
        .find('?')
        .map(|index| &database_and_query[index..])
        .unwrap_or_default();
    Ok(format!("{prefix}/{database_name}{query}"))
}

fn spawn_session_turn_waiter_probe_run(
    client: &reqwest::Client,
    ingress: &str,
    probe_key: &str,
    session_id: SessionId,
    turn_id: &str,
) -> JoinHandle<Result<TurnOutcome>> {
    let client = client.clone();
    let ingress = ingress.to_string();
    let service_or_object = format!("SessionTurnWaiterProbe/{probe_key}");
    let input = SessionTurnWaiterProbeInput {
        session_id,
        turn_id: turn_id.to_string(),
    };
    tokio::spawn(async move {
        post_json(&client, &ingress, &service_or_object, "run", &input)
            .await?
            .json::<TurnOutcome>()
            .await
            .context("deserialize session turn waiter probe run response")
    })
}

async fn wait_for_session_turn_waiter_probe_attached(
    client: &reqwest::Client,
    ingress: &str,
    probe_key: &str,
) -> Result<()> {
    let service_or_object = format!("SessionTurnWaiterProbe/{probe_key}");
    let mut last_observation = "not yet checked".to_string();
    for _attempt in 0..60 {
        match post_empty(client, ingress, &service_or_object, "attached").await {
            Ok(response) => {
                let attached = response
                    .json::<bool>()
                    .await
                    .context("deserialize session turn waiter probe attached response")?;
                if attached {
                    return Ok(());
                }
                last_observation = "probe not attached yet".to_string();
            }
            Err(error) => {
                last_observation = error.to_string();
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!(
        "timed out waiting for session turn waiter probe {probe_key} to attach: {last_observation}"
    )
}

async fn wait_for_session_turn_waiter_probe_outcome(
    probe_task: &mut JoinHandle<Result<TurnOutcome>>,
) -> Result<TurnOutcome> {
    tokio::time::timeout(Duration::from_secs(30), probe_task)
        .await
        .context("timed out waiting for session turn waiter probe outcome")?
        .context("join session turn waiter probe task")?
}

async fn post_json<T: serde::Serialize + ?Sized>(
    client: &reqwest::Client,
    ingress: &str,
    service_or_object: &str,
    handler: &str,
    request: &T,
) -> Result<reqwest::Response> {
    let response = client
        .post(format!(
            "{}/restate/call/{service_or_object}/{handler}",
            ingress.trim_end_matches('/')
        ))
        .json(request)
        .send()
        .await
        .with_context(|| format!("call {service_or_object}/{handler}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    bail!("{service_or_object}/{handler} returned {status}: {body}")
}

async fn post_empty(
    client: &reqwest::Client,
    ingress: &str,
    service_or_object: &str,
    handler: &str,
) -> Result<reqwest::Response> {
    let response = client
        .post(format!(
            "{}/restate/call/{service_or_object}/{handler}",
            ingress.trim_end_matches('/')
        ))
        .send()
        .await
        .with_context(|| format!("call {service_or_object}/{handler}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    bail!("{service_or_object}/{handler} returned {status}: {body}")
}

async fn post_json_with_identity<T: serde::Serialize + ?Sized>(
    client: &reqwest::Client,
    ingress: &str,
    service_or_object: &str,
    handler: &str,
    identity: &Identity,
    request: &T,
) -> Result<reqwest::Response> {
    let response = with_identity(
        client.post(format!(
            "{}/restate/call/{service_or_object}/{handler}",
            ingress.trim_end_matches('/')
        )),
        identity,
    )
    .json(request)
    .send()
    .await
    .with_context(|| format!("call {service_or_object}/{handler}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    bail!("{service_or_object}/{handler} returned {status}: {body}")
}

fn assert_persisted_trial_matches_response(
    trial: &ExperimentTrialRecord,
    response: &ExperimentTrialRunStatusResponse,
    session_id: SessionId,
) {
    assert_eq!(trial.trial_uid, response.trial_uid);
    assert_eq!(trial.run_uid, response.run_uid);
    assert_eq!(trial.trial_key, response.trial_key);
    assert_eq!(trial.status.as_str(), response.status);
    assert_eq!(
        trial.stop_reason.map(|reason| reason.as_str().to_string()),
        response.stop_reason
    );
    assert_eq!(trial.turn_count, response.turn_count);
    assert_eq!(trial.session_id, Some(session_id));
    assert_eq!(trial.score_run_id, response.score_run_id);
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

fn new_parent_run(identity: &Identity, agent_revision_uid: Uuid) -> NewExperimentRun {
    NewExperimentRun {
        name: "scripted trial workflow".to_string(),
        target: serde_json::from_value(agent_loop_target(agent_revision_uid))
            .expect("target fixture should parse"),
        variant: serde_json::from_value(baseline_variant()).expect("variant fixture should parse"),
        scorecard: ExperimentScorecard {
            score_names: vec!["task_success".to_string()],
            evaluator_metadata: json!({ "judge": "manual-or-later" }),
        },
        score_run_id: Uuid::now_v7(),
        session_id: None,
        procedure_run_uid: None,
        artifact_revision_uids: Vec::new(),
        idempotency_key: Some(format!("trial-parent-{}", Uuid::now_v7())),
        created_by_identity: json!({
            "type": "operator",
            "id": identity.id.to_string(),
        }),
    }
}

async fn publish_trial_agent(pool: &PgPool, scope: &ActionRuleScope) -> Result<Uuid> {
    let source = r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: scripted-trial-agent
  description: Agent fixture for deterministic trial workflow coverage.
status: draft
definition:
  type: agent
  spec:
    display_name: Scripted Trial Agent
    purpose:
      summary: Handle deterministic trial prompts.
      default_task: Follow the scripted trial prompt.
      expected_outputs:
        - support response
    instruction_policy:
      system_prompt: You are a deterministic support agent for trial workflow tests.
"#;
    publish_artifact_revision(pool, scope, source, "trial agent").await
}

async fn publish_trial_plan(
    pool: &PgPool,
    scope: &ActionRuleScope,
    agent_revision_uid: Uuid,
) -> Result<Uuid> {
    let source = format!(
        r#"
api_version: moa.artifact/v1
kind: experiment_plan
metadata:
  name: scripted-trial-plan
  description: Scripted trial workflow fixture.
status: draft
definition:
  type: experiment_plan
  spec:
    simulation:
      scenarios:
        - id: delayed-order
          initial_situation: The user needs help with a delayed order.
          goals:
            - Get a concrete order status next step.
          success_criteria:
            - The target gives a concrete next step.
          max_turns: 2
      personas:
        - id: careful-customer
          voice: Patient and concise.
          goals:
            - Resolve the delayed order.
          stop_behavior: Stop after the target gives a concrete next step.
      profiles:
        - id: order-profile
          facts:
            order_id: ORDER-42
    target_variants:
      - key: baseline
        kind: agent_loop
        config:
          prompt: Start behavior-lab simulation.
          agent_revision_uid: "{agent_revision_uid}"
    simulator_model: scripted-loadtest
    target_model: scripted-loadtest
    parallelism: 1
    trials_per_combination: 1
    budget:
      max_total_cents: 1
      max_trial_tokens: 1000
"#
    );
    publish_artifact_revision(pool, scope, source.as_str(), "trial plan").await
}

async fn publish_artifact_revision(
    pool: &PgPool,
    scope: &ActionRuleScope,
    source: &str,
    label: &str,
) -> Result<Uuid> {
    let document = ArtifactDocument::from_yaml(source).context("parse trial plan artifact")?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        bail!("{label} artifact should validate: {:?}", report.errors);
    }
    let registry = ArtifactRegistry::new(pool.clone());
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await
        .with_context(|| format!("create {label} draft"))?;
    let published = registry
        .publish_revision(scope, draft.revision_uid, &report)
        .await
        .with_context(|| format!("publish {label} revision"))?;
    Ok(published.revision_uid)
}

fn new_trial(run_uid: Uuid, plan_revision_uid: Uuid) -> NewExperimentTrial {
    NewExperimentTrial {
        run_uid,
        trial_key: "scripted-multiturn-agent-loop".to_string(),
        target_kind: ExperimentTargetKind::AgentLoop,
        variant_key: "baseline".to_string(),
        plan_revision_uid,
        scenario_id: Some("delayed-order".to_string()),
        persona_id: Some("careful-customer".to_string()),
        profile_id: Some("order-profile".to_string()),
        data_bundle_ids: Vec::new(),
        artifact_revision_uids: Vec::new(),
        simulator: ExperimentSimulatorConfig {
            model: ModelId::new("scripted-loadtest"),
            temperature: Some(0.0),
            max_turns: 2,
            token_budget: Some(1_000),
            metadata: json!({ "fixture": "experiment_trial_run_e2e" }),
        },
        target_model: Some(ModelId::new("scripted-loadtest")),
        seed: Some("scripted-trial-seed".to_string()),
        score_run_id: Uuid::now_v7(),
    }
}

fn agent_loop_target(agent_revision_uid: Uuid) -> Value {
    json!({
        "kind": "agent_loop",
        "prompt": "Run the delayed-order support behavior trial.",
        "session_id": null,
        "agent": { "revision_uid": agent_revision_uid },
        "model": "scripted-loadtest",
        "attachments": []
    })
}

fn baseline_variant() -> Value {
    json!({
        "name": "baseline",
        "model": "scripted-loadtest",
        "artifact_revision_uids": [],
        "skill_refs": [],
        "procedure_ref": null,
        "metadata": { "lane": "experiment_trial_run_e2e" }
    })
}

fn new_procedure_trial(run_uid: Uuid, plan_revision_uid: Uuid) -> NewExperimentTrial {
    NewExperimentTrial {
        run_uid,
        trial_key: "procedure-review-trial".to_string(),
        target_kind: ExperimentTargetKind::Procedure,
        variant_key: "baseline".to_string(),
        plan_revision_uid,
        scenario_id: Some("delayed-order".to_string()),
        persona_id: Some("careful-customer".to_string()),
        profile_id: Some("order-profile".to_string()),
        data_bundle_ids: Vec::new(),
        artifact_revision_uids: Vec::new(),
        simulator: ExperimentSimulatorConfig {
            model: ModelId::new("scripted-loadtest"),
            temperature: Some(0.0),
            max_turns: 2,
            token_budget: Some(1_000),
            metadata: json!({ "fixture": "experiment_trial_run_e2e_procedure" }),
        },
        target_model: None,
        seed: Some("procedure-review-seed".to_string()),
        score_run_id: Uuid::now_v7(),
    }
}

fn procedure_review_target() -> Value {
    json!({
        "kind": "procedure",
        "procedure_ref": "skill://review-gated-procedure",
        "input": {},
        "session_id": null,
        "idempotency_key": null
    })
}

fn spawn_procedure_trial_run(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    run_uid: Uuid,
    trial_key: &str,
    request: &ExperimentTrialRunWorkflowRequest,
) -> JoinHandle<Result<ExperimentTrialRunStatusResponse>> {
    let client = client.clone();
    let ingress = ingress.to_string();
    let identity = identity.clone();
    let key = trial_workflow_key(run_uid, trial_key);
    let request = request.clone();
    tokio::spawn(async move {
        post_json_with_identity(
            &client,
            &ingress,
            &format!("ExperimentTrialRun/{key}"),
            "run",
            &identity,
            &request,
        )
        .await?
        .json::<ExperimentTrialRunStatusResponse>()
        .await
        .context("deserialize backgrounded ExperimentTrialRun/run response")
    })
}

async fn wait_for_trial_procedure_link(
    store: &ExperimentStore,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    trial_key: &str,
) -> Result<ExperimentTrialRecord> {
    for _attempt in 0..60 {
        if let Some(trial) = store
            .load_trial_by_key(scope, run_uid, trial_key)
            .await
            .context("load trial while waiting for procedure link")?
            && trial.procedure_run_uid.is_some()
        {
            return Ok(trial);
        }
        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for trial {trial_key} to link a procedure run")
}

async fn wait_for_procedure_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
    expected: &str,
) -> Result<ProcedureRunStatus> {
    let request = ProcedureStatusRequest { tenant_id, run_id };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Skills", "status", identity, &request)
                .await?
                .json::<ProcedureRunStatus>()
                .await
                .context("deserialize procedure status response")?;
        if status.status == expected {
            return Ok(status);
        }
        if status.status == "failed" {
            bail!("procedure run {run_id} failed before reaching {expected}: {status:?}");
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for procedure run {run_id} to reach {expected}; last: {last_status:?}")
}

async fn decide_procedure_review(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<ProcedureReviewDecisionResponse> {
    let request = ProcedureReviewDecisionRequest {
        tenant_id,
        run_id,
        node_id: Some("gate".to_string()),
        decision: ProcedureReviewDecisionKind::Approved,
        reason: Some("approved in procedure trial e2e".to_string()),
        output: Some(json!({ "approved": true })),
    };
    post_json_with_identity(
        client,
        ingress,
        "Skills",
        "decide_review",
        identity,
        &request,
    )
    .await?
    .json::<ProcedureReviewDecisionResponse>()
    .await
    .context("deserialize procedure review decision response")
}

fn review_gated_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: review-gated-procedure
  description: Procedure that pauses on an explicit review node.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
      nodes:
        - id: start
          kind: start
          ui:
            x: 80
            y: 120
        - id: gate
          kind: review
          input:
            prompt: Approve before completing the procedure.
          ui:
            x: 280
            y: 120
        - id: done
          kind: end
          input:
            reviewed: true
          ui:
            x: 520
            y: 120
      edges:
        - id: start-gate
          from: start
          to: gate
        - id: gate-done
          from: gate
          to: done
      ui:
        layout: dagre
"#
}

fn write_scripted_fixture(path: &Path) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": "DONE",
                "tool_calls": []
            }
        },
        "responses": [
            {
                "completion": {
                    "content": "Hi, I need help with a delayed order.",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "I can help with the delayed order. What is your order id?",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "The order id is ORDER-42. Can you check it?",
                    "tool_calls": []
                }
            },
            {
                "completion": {
                    "content": "Thanks, I checked ORDER-42 and the delivery support case is ready for follow-up.",
                    "tool_calls": []
                }
            }
        ]
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted fixture")?;
    fs::write(path, body).context("write scripted fixture")
}
