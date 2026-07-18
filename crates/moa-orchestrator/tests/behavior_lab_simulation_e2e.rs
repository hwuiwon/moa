//! Execution-template experiment-run delivery coverage through Restate.

#![cfg(feature = "integration")]

use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use moa_core::{
    events::Event,
    traits::Identity,
    types::{
        action_policy::ActionRuleScope,
        contact::ContactId,
        events_stream::{EventRange, EventRecord},
        execution_planning::{ExecutionRunStarted, PinnedExecutionTemplateRef},
        identifiers::{ModelId, SessionId, TenantId},
    },
    wire::{
        artifacts::{
            ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest,
            ArtifactPublishResponse,
        },
        experiments::{ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusResponse},
    },
};
use moa_experiments::{
    model::{ExperimentScorecard, ExperimentTarget, ExperimentVariant, NewExperimentRun},
    store::ExperimentStore,
};
use moa_orchestrator::workflows::experiment_run::ExperimentRunWorkflowRequest;
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

#[path = "support/mod.rs"]
mod support;

const EXPERIMENT_EXECUTION_SESSION_NAMESPACE: Uuid =
    Uuid::from_u128(0xc2a6_731c_2d80_5d4a_9d10_2d20_1283_c6ec);
const EXPERIMENT_EXECUTION_SESSION_DOMAIN: &str = "moa.experiment.execution-session";
const TEMPLATE_SKILL_REF: &str = "skill://experiment-resolution";

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
        .env("MOA_CLOUD_HANDS_ALLOW_LOCAL", "true")
        .env("MOA_RUNTIME_CACHE_REDIS_URL", "redis://127.0.0.1:10051/0")
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
        .context("spawn moa-orchestrator binary for execution-template experiment e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, Valkey, and provider-overrides feature"]
async fn execution_template_run_target_tenant_scoped_internal_session() -> Result<()> {
    // Pins: a tenant-scoped sessionless execution-template experiment creates one deterministic
    // internal Session and delivers its objective, admitted run, and terminal result as typed
    // Session events without polling an execution lifecycle API.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("tenant-template-experiment.json");
    write_scripted_fixture(&fixture_path)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let published = publish_execution_template(&client, ingress, &identity, scope).await?;
        let objective = "Resolve tenant case TENANT-42 through the pinned execution template.";
        let (target, variant) = execution_template_fixture(
            published.revision_uid,
            objective,
            json!({"case_id": "TENANT-42", "resolution": "replacement"}),
        );
        let response =
            run_tenant_experiment(&client, ingress, &identity, tenant_id, &target, &variant)
                .await?;
        assert_eq!(response.status, "accepted");
        assert_eq!(response.session_id, None);
        assert_eq!(response.execution_run_uid, None);

        let session_id =
            experiment_execution_session_id(tenant_id, response.run_uid, response.score_run_id);
        let delivery =
            wait_for_execution_delivery(&client, ingress, &identity, session_id, objective).await?;

        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        assert_internal_session_scope(&pool, session_id, tenant_id, None, &identity).await?;
        assert_execution_links(
            &pool,
            response.run_uid,
            session_id,
            delivery.started.run_uid,
            tenant_id,
            None,
            delivery.objective_sequence,
        )
        .await?;
        pool.close().await;

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, Valkey, and provider-overrides feature"]
async fn execution_template_run_target_contact_scoped_internal_session() -> Result<()> {
    // Pins: a contact-scoped sessionless execution-template experiment preserves the exact
    // contact on its deterministic internal Session and common ExecutionRun while delivering the
    // objective and lifecycle through typed Session events.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("contact-template-experiment.json");
    write_scripted_fixture(&fixture_path)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let scope = ActionRuleScope::Contact {
        tenant_id,
        contact_id,
    };
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let published = publish_execution_template(&client, ingress, &identity, scope).await?;
        let objective = "Resolve contact case CONTACT-42 without widening its storage scope.";
        let (target, variant) = execution_template_fixture(
            published.revision_uid,
            objective,
            json!({"case_id": "CONTACT-42", "resolution": "credit"}),
        );
        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        let score_run_id = Uuid::now_v7();
        let run = ExperimentStore::new(pool.clone())
            .insert_run(
                &scope,
                NewExperimentRun {
                    name: "contact-scoped-template-experiment".to_string(),
                    target: target.clone(),
                    variant: variant.clone(),
                    scorecard: experiment_scorecard(),
                    score_run_id,
                    session_id: None,
                    execution_run_uid: None,
                    artifact_revision_uids: vec![published.revision_uid],
                    idempotency_key: Some(format!(
                        "contact-template-experiment-{}",
                        Uuid::now_v7()
                    )),
                    created_by_identity: serde_json::to_value(&identity)
                        .context("serialize experiment creator identity")?,
                },
            )
            .await
            .context("seed contact-scoped execution-template experiment")?;
        let workflow_request = ExperimentRunWorkflowRequest {
            tenant_id,
            run_uid: run.run_uid,
            target: serde_json::to_value(&target).context("serialize experiment target")?,
            variant: serde_json::to_value(&variant).context("serialize experiment variant")?,
            plan_revision_uid: None,
            identity: identity.clone(),
            score_run_id,
            agent_revision_variants: Vec::new(),
        };
        let workflow_service = format!("ExperimentRun/{}", run.run_uid);
        let workflow_response = post_json_with_identity(
            &client,
            ingress,
            &workflow_service,
            "run",
            &identity,
            &workflow_request,
        )
        .await?
        .json::<ExperimentRunStatusResponse>()
        .await
        .context("deserialize contact-scoped ExperimentRun response")?;
        assert_eq!(
            workflow_response.target_kind.as_deref(),
            Some("execution_template")
        );

        let session_id = experiment_execution_session_id(tenant_id, run.run_uid, score_run_id);
        let delivery =
            wait_for_execution_delivery(&client, ingress, &identity, session_id, objective).await?;
        assert_internal_session_scope(&pool, session_id, tenant_id, Some(contact_id), &identity)
            .await?;
        assert_execution_links(
            &pool,
            run.run_uid,
            session_id,
            delivery.started.run_uid,
            tenant_id,
            Some(contact_id),
            delivery.objective_sequence,
        )
        .await?;
        pool.close().await;

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

struct ExecutionDelivery {
    objective_sequence: u64,
    started: ExecutionRunStarted,
}

async fn publish_execution_template(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    scope: ActionRuleScope,
) -> Result<ArtifactPublishResponse> {
    let imported = post_json_with_identity(
        client,
        ingress,
        "Artifacts",
        "import",
        identity,
        &ArtifactImportRequest {
            scope,
            source_format: "yaml".to_string(),
            source_text: execution_template_source().to_string(),
            files: Vec::new(),
        },
    )
    .await?
    .json::<ArtifactImportResponse>()
    .await
    .context("deserialize execution-template artifact import")?;
    assert_eq!(imported.status, "draft");

    let published = post_json_with_identity(
        client,
        ingress,
        "Artifacts",
        "publish",
        identity,
        &ArtifactPublishRequest {
            scope,
            revision_uid: imported.revision_uid,
        },
    )
    .await?
    .json::<ArtifactPublishResponse>()
    .await
    .context("deserialize execution-template artifact publish")?;
    assert_eq!(published.status, "published");
    assert_validation_report_has_no_errors(&published.validation_report)?;
    Ok(published)
}

fn execution_template_fixture(
    revision_uid: Uuid,
    objective: &str,
    input: Value,
) -> (ExperimentTarget, ExperimentVariant) {
    let template = PinnedExecutionTemplateRef {
        skill_ref: TEMPLATE_SKILL_REF.to_string(),
        revision_uid,
    };
    (
        ExperimentTarget::ExecutionTemplate {
            template: template.clone(),
            objective: objective.to_string(),
            input,
            session_id: None,
            idempotency_key: Some(format!("template-target-{}", Uuid::now_v7())),
        },
        ExperimentVariant {
            name: "pinned-template".to_string(),
            model: Some(ModelId::new("scripted-loadtest")),
            artifact_revision_uids: vec![revision_uid],
            skill_refs: Vec::new(),
            execution_template: Some(template),
            metadata: json!({"lane": "execution-template-experiment-e2e"}),
        },
    )
}

async fn run_tenant_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    target: &ExperimentTarget,
    variant: &ExperimentVariant,
) -> Result<ExperimentRunResponse> {
    let request = ExperimentRunRequest {
        tenant_id,
        name: "tenant-scoped-template-experiment".to_string(),
        plan_revision_uid: None,
        target: Some(serde_json::to_value(target).context("serialize experiment target")?),
        variant: Some(serde_json::to_value(variant).context("serialize experiment variant")?),
        scorecard: serde_json::to_value(experiment_scorecard())
            .context("serialize experiment scorecard")?,
        score_run_id: None,
        idempotency_key: Some(format!("tenant-template-experiment-{}", Uuid::now_v7())),
        agent_revision_variants: Vec::new(),
    };
    post_json_with_identity(client, ingress, "Experiments", "run", identity, &request)
        .await?
        .json::<ExperimentRunResponse>()
        .await
        .context("deserialize execution-template experiment admission")
}

fn experiment_scorecard() -> ExperimentScorecard {
    ExperimentScorecard {
        score_names: vec!["template_completed".to_string()],
        evaluator_metadata: json!({"mode": "manual-or-later"}),
    }
}

async fn wait_for_execution_delivery(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    objective: &str,
) -> Result<ExecutionDelivery> {
    let mut last_events = Vec::new();
    for _attempt in 0..90 {
        let events = fetch_events(client, ingress, identity, session_id).await?;
        let objectives = events
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::UserMessage { text, .. } if text == objective)
            })
            .collect::<Vec<_>>();
        let starts = events
            .iter()
            .filter_map(|record| match &record.event {
                Event::ExecutionRunStarted(started) => Some((record.sequence_num, started)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let completions = events
            .iter()
            .filter_map(|record| match &record.event {
                Event::ExecutionCompleted(summary) => Some((record.sequence_num, summary.run_uid)),
                _ => None,
            })
            .collect::<Vec<_>>();

        if objectives.len() > 1 || starts.len() > 1 || completions.len() > 1 {
            bail!(
                "execution delivery duplicated for session {session_id}: {}",
                summarize_events(&events)
            );
        }
        if let ([objective_event], [(started_sequence, started)], [(completed_sequence, run_uid)]) = (
            objectives.as_slice(),
            starts.as_slice(),
            completions.as_slice(),
        ) {
            assert_eq!(
                started.originating_user_sequence_num,
                objective_event.sequence_num
            );
            assert_eq!(*run_uid, started.run_uid);
            assert!(objective_event.sequence_num < *started_sequence);
            assert!(*started_sequence < *completed_sequence);
            return Ok(ExecutionDelivery {
                objective_sequence: objective_event.sequence_num,
                started: (*started).clone(),
            });
        }
        last_events = events;
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for typed execution delivery in session {session_id}; observed events: {}",
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

async fn assert_internal_session_scope(
    pool: &PgPool,
    session_id: SessionId,
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    identity: &Identity,
) -> Result<()> {
    let row = sqlx::query_as::<_, (Uuid, Option<Uuid>, Option<String>, Option<Uuid>)>(
        r#"
        SELECT tenant_id, contact_id, created_by_actor_type, created_by_actor_id
        FROM sessions
        WHERE id = $1
        "#,
    )
    .bind(session_id.0)
    .fetch_one(pool)
    .await
    .context("load deterministic internal experiment session")?;
    assert_eq!(row.0, tenant_id.0);
    assert_eq!(row.1, contact_id.map(|value| value.0));
    assert_eq!(row.2.as_deref(), Some("identity"));
    assert_eq!(
        row.3,
        Some(identity.acting_on_behalf_of.unwrap_or(identity.id))
    );
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the assertion keeps every persisted execution-scope dimension explicit"
)]
async fn assert_execution_links(
    pool: &PgPool,
    experiment_run_uid: Uuid,
    session_id: SessionId,
    execution_run_uid: Uuid,
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    objective_sequence: u64,
) -> Result<()> {
    let experiment = sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>, String)>(
        r#"
        SELECT session_id, execution_run_uid, target_kind
        FROM moa.experiment_run
        WHERE run_uid = $1
        "#,
    )
    .bind(experiment_run_uid)
    .fetch_one(pool)
    .await
    .context("load experiment execution links")?;
    assert_eq!(experiment.0, Some(session_id.0));
    assert_eq!(experiment.1, Some(execution_run_uid));
    assert_eq!(experiment.2, "execution_template");

    let execution = sqlx::query_as::<_, (Uuid, Option<Uuid>, Uuid, i64)>(
        r#"
        SELECT tenant_id, contact_id, session_id, originating_user_sequence_num
        FROM moa.execution_run
        WHERE run_uid = $1
        "#,
    )
    .bind(execution_run_uid)
    .fetch_one(pool)
    .await
    .context("load common execution run linked by experiment")?;
    assert_eq!(execution.0, tenant_id.0);
    assert_eq!(execution.1, contact_id.map(|value| value.0));
    assert_eq!(execution.2, session_id.0);
    assert_eq!(execution.3, i64::try_from(objective_sequence)?);
    Ok(())
}

fn experiment_execution_session_id(
    tenant_id: TenantId,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
) -> SessionId {
    let mut name = EXPERIMENT_EXECUTION_SESSION_DOMAIN.as_bytes().to_vec();
    append_nullable_frame(&mut name, Some(tenant_id.to_string().as_bytes()));
    append_nullable_frame(&mut name, Some(experiment_run_uid.to_string().as_bytes()));
    append_nullable_frame(&mut name, Some(score_run_id.to_string().as_bytes()));
    append_nullable_frame(&mut name, None);
    SessionId(Uuid::new_v5(&EXPERIMENT_EXECUTION_SESSION_NAMESPACE, &name))
}

fn append_nullable_frame(output: &mut Vec<u8>, value: Option<&[u8]>) {
    let Some(value) = value else {
        output.push(0);
        return;
    };
    output.push(1);
    output.extend_from_slice(
        &u32::try_from(value.len())
            .expect("UUID identity frame should fit in u32")
            .to_be_bytes(),
    );
    output.extend_from_slice(value);
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
    format!(
        "{}/restate/call/{service}/{handler}",
        ingress.trim_end_matches('/')
    )
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

fn write_scripted_fixture(path: &Path) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": "The pinned experiment execution completed.",
                "tool_calls": []
            }
        }
    });
    fs::write(
        path,
        serde_json::to_vec_pretty(&fixture).context("serialize scripted fixture")?,
    )
    .context("write scripted fixture")
}

fn execution_template_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: experiment-resolution
  description: Deterministic execution template for experiment-run delivery tests.
status: draft
definition:
  type: skill
  spec:
    inputs:
      type: object
      additionalProperties: false
      required: [case_id, resolution]
      properties:
        case_id: { type: string }
        resolution: { type: string }
    execution_plan:
      goal:
        requirements:
          - id: req_resolution
            description: Persist the requested case resolution.
        deliverables: []
        coverage: []
        constraints: []
        completion_checks:
          - id: check_output
            description: Validate the exact structured resolution output.
            requirement_ids: [req_resolution]
            constraint_ids: []
            kind:
              kind: output_schema
      plan:
        schema_version: 1
        input_schema:
          type: object
          additionalProperties: false
          required: [case_id, resolution]
          properties:
            case_id: { type: string }
            resolution: { type: string }
        output_schema:
          type: object
          additionalProperties: false
          required: [case_id, resolution]
          properties:
            case_id: { type: string }
            resolution: { type: string }
        nodes:
          - id: output
            requirement_ids: [req_resolution]
            depends_on: []
            input: {}
            output_schema:
              type: object
              additionalProperties: false
              required: [case_id, resolution]
              properties:
                case_id: { type: string }
                resolution: { type: string }
            operation:
              kind: output
              value:
                case_id:
                  $ref: $.input.case_id
                resolution:
                  $ref: $.input.resolution
            retry:
              max_attempts: 1
              initial_backoff_ms: 0
              max_backoff_ms: 0
"#
}
