//! End-to-end coverage for behavior-lab trial execution through Restate.

#![cfg(feature = "integration")]

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
    ActionRuleScope, Event, EventRange, EventRecord, ModelId, SessionId, TenantId, traits::Identity,
};
use moa_experiments::{
    model::{
        ExperimentScorecard, ExperimentSimulatorConfig, ExperimentTrialRecord, NewExperimentRun,
        NewExperimentTrial,
    },
    store::ExperimentStore,
};
use moa_orchestrator::workflows::experiment_trial_run::{
    ExperimentTrialRunStatusResponse, ExperimentTrialRunWorkflowRequest, trial_workflow_key,
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

mod support {
    pub mod grant_tenant_admin;
    pub mod restate_admin_url;
    pub mod restate_identity;
    pub mod restate_ingress_url;
    pub mod restate_lock;
    pub mod restate_ports;
    pub mod restate_register;
    pub mod session_get_events;

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

    pub mod session_store_service {
        pub use super::session_get_events::get_events_request;
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
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for trial e2e")
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
        let run = store
            .insert_run(&scope, new_parent_run(&identity))
            .await
            .context("seed parent experiment run")?;
        let plan_revision_uid = publish_trial_plan(&pool, &scope).await?;
        let trial = new_trial(run.run_uid, plan_revision_uid);
        let trial_key = trial.trial_key.clone();
        let workflow_request = ExperimentTrialRunWorkflowRequest {
            tenant_id,
            trial: trial.clone(),
            target: agent_loop_target(),
            variant: baseline_variant(),
            identity: identity.clone(),
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

        let retry = run_trial_workflow(
            &client,
            ingress,
            &identity,
            run.run_uid,
            &trial_key,
            &workflow_request,
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
            "{}/{service_or_object}/{handler}",
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

fn new_parent_run(identity: &Identity) -> NewExperimentRun {
    NewExperimentRun {
        name: "scripted trial workflow".to_string(),
        target: serde_json::from_value(agent_loop_target()).expect("target fixture should parse"),
        variant: serde_json::from_value(baseline_variant()).expect("variant fixture should parse"),
        scorecard: ExperimentScorecard {
            score_names: vec!["task_success".to_string()],
            evaluator_metadata: json!({ "judge": "manual-or-later" }),
        },
        score_run_id: Uuid::now_v7(),
        session_id: None,
        workflow_run_uid: None,
        artifact_revision_uids: Vec::new(),
        idempotency_key: Some(format!("trial-parent-{}", Uuid::now_v7())),
        created_by_identity: json!({
            "type": "user",
            "id": identity.id.to_string(),
        }),
    }
}

async fn publish_trial_plan(pool: &PgPool, scope: &ActionRuleScope) -> Result<Uuid> {
    let source = r#"
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
    simulator_model: scripted-loadtest
    target_model: scripted-loadtest
    parallelism: 1
    trials_per_combination: 1
    budget:
      max_total_cents: 1
      max_trial_tokens: 1000
"#;
    let document = ArtifactDocument::from_yaml(source).context("parse trial plan artifact")?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        bail!("trial plan artifact should validate: {:?}", report.errors);
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
        .context("create trial plan draft")?;
    let published = registry
        .publish_revision(scope, draft.revision_uid, &report)
        .await
        .context("publish trial plan revision")?;
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

fn agent_loop_target() -> Value {
    json!({
        "kind": "agent_loop",
        "prompt": "Run the delayed-order support behavior trial.",
        "session_id": null,
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
        "workflow_ref": null,
        "metadata": { "lane": "experiment_trial_run_e2e" }
    })
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
