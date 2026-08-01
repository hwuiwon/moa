//! End-to-end coverage for agent-loop experiment target execution through Restate.

#![cfg(feature = "integration")]

use moa_core::types::experiments::{ExperimentScorecard, ScorecardEffect, ScorecardRequirement};
use std::time::Duration;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::document::ArtifactStatus;
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::release::TenantScope;
use moa_core::traits::Identity;
use moa_core::{
    events::Event, types::action_policy::ActionRuleScope, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use moa_skills::artifact::{SKILL_ARTIFACT_PATH, skill_artifact_document_from_package};
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::test_database_url;
use moa_wire::artifacts::{
    ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest, ArtifactPublishResponse,
};
use moa_wire::experiments::{
    ExperimentListRequest, ExperimentListResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentRunStatusRequest, ExperimentRunStatusResponse,
};
use moa_wire::skills::{SkillPackageDocument, SkillPackageDocumentFile};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_tenant_admin,
    register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
    test_user_identity, with_identity,
};
use crate::support::session_store_service::get_events_request;

#[path = "support/mod.rs"]
mod support;

const SUPPORT_SKILL_PATH: &str = ".moa/skills/delivery-support/SKILL.md";
const SUPPORT_SKILL_PROVIDER_ID: &str = "read_delivery_support_skill";

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
        .env("MOA_SECURITY_PROFILE", "local")
        // Opt into the ephemeral in-process KMS so the fail-closed durability
        // guard permits startup (production uses a persistent postgres KMS).
        .env("MOA_KMS_ALLOW_EPHEMERAL", "true")
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
        .context("spawn moa-orchestrator binary for experiment e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn agent_loop_experiment_creates_session_and_persists_scripted_response() -> Result<()> {
    // Pins: ExperimentRun drives Session/TurnExecution and links the created session to the run.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("experiment-agent-loop-script.json");
    let final_text = "The delivery support runbook says this spilled order qualifies for a replacement after confirming the photo and order id.";
    write_scripted_fixture(&fixture_path, final_text)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        activate_support_skill_fixture(&storage_partition_id).await?;
        let agent = import_and_validate_agent_candidate(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            support_agent_source(),
        )
        .await?;

        let run = run_agent_loop_experiment(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            agent.revision_uid,
        )
        .await?;
        assert_eq!(run.status, "accepted");
        assert_ne!(run.score_run_id, Uuid::nil());
        assert!(
            run.session_id.is_none(),
            "session should be attached by the workflow after acceptance"
        );
        assert!(
            run.execution_run_uid.is_none(),
            "agent-loop experiments must not attach a detached execution run"
        );

        let status =
            wait_for_experiment_status(&client, ingress, &identity, &storage_partition_id, run.run_uid)
                .await?;
        assert_eq!(status.score_run_id, Some(run.score_run_id));
        assert!(status.execution_run_uid.is_none());
        assert!(
            matches!(status.status.as_str(), "completed"),
            "experiment should complete without a blocking review state, got {status:?}"
        );
        let session_id = status
            .session_id
            .context("experiment status should expose linked session_id")?;

        let events =
            wait_for_brain_response_text(&client, ingress, &identity, session_id, final_text)
                .await?;
        assert!(
            saw_successful_skill_file_read(&events),
            "expected the approved learned skill to be read through normal tool routing; observed events: {}",
            summarize_events(&events)
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
async fn experiments_run_denies_caller_without_tenant_operator() -> Result<()> {
    // Pins: Experiments/run authorizes before target admission, so a tenant-scoped caller with no
    // operator/admin grant receives the exact forbidden status and creates no experiment run.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir
        .path()
        .join("unauthorized-experiment-script.json");
    write_scripted_fixture(&fixture_path, "unreachable")?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut unauthorized = test_user_identity();
    unauthorized.tenant_id = tenant_id;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let request = ExperimentRunRequest {
            tenant_id,
            name: "unauthorized-agent-loop-experiment".to_string(),
            plan_revision_uid: None,
            target: Some(json!({
                "kind": "agent_loop",
                "prompt": "This target must never be admitted.",
                "session_id": null,
                "agent": { "revision_uid": Uuid::now_v7() },
                "model": "scripted-loadtest",
                "attachments": []
            })),
            variant: Some(json!({
                "name": "unauthorized-agent-loop",
                "model": "scripted-loadtest",
                "artifact_revision_uids": [],
                "skill_refs": [],
                "execution_template": null,
                "metadata": {}
            })),
            scorecard: Some(fixture_scorecard()),
            score_run_id: None,
            idempotency_key: Some(format!("unauthorized-agent-loop-{}", Uuid::now_v7())),
            agent_revision_variants: Vec::new(),
            release_evaluation: None,
        };

        let response = with_identity(
            client.post(service_url(ingress, "Experiments", "run")),
            &unauthorized,
        )
        .json(&request)
        .send()
        .await
        .context("call Experiments/run without tenant operator grant")?;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn experiments_run_rejects_unreachable_execution_template_session_before_admission()
-> Result<()> {
    // Pins: an execution-template session the caller cannot reach is refused by
    // the Session participant check, and the refusal lands before any admission
    // write, dispatch, or durable event read. Agent-loop targets are not covered
    // here because they carry no session field at all — that is pinned at the
    // type level by the `Experiments` service and plan-expansion unit tests.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir
        .path()
        .join("target-session-experiment-script.json");
    write_scripted_fixture(&fixture_path, "unreachable")?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    grant_tenant_admin(&identity, tenant_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let foreign_session_id = Uuid::now_v7();
        let template_revision_uid = Uuid::now_v7();
        let unreachable_session = run_request_with_target(
            tenant_id,
            "execution-template-foreign-session",
            json!({
                "kind": "execution_template",
                "template": {
                    "skill_ref": "skill://durable-report",
                    "revision_uid": template_revision_uid,
                },
                "objective": "produce the durable report",
                "input": {},
                "session_id": foreign_session_id,
                "idempotency_key": null
            }),
            json!({
                "name": "execution-template-foreign-session",
                "model": "scripted-loadtest",
                "artifact_revision_uids": [],
                "skill_refs": [],
                "execution_template": {
                    "skill_ref": "skill://durable-report",
                    "revision_uid": template_revision_uid,
                },
                "metadata": {}
            }),
        );
        let response = with_identity(
            client.post(service_url(ingress, "Experiments", "run")),
            &identity,
        )
        .json(&unreachable_session)
        .send()
        .await
        .context("call Experiments/run with an unreachable execution-template session")?;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

        let listed = post_json_with_identity(
            &client,
            ingress,
            "Experiments",
            "list",
            &identity,
            &ExperimentListRequest {
                tenant_id,
                status: None,
                limit: Some(10),
            },
        )
        .await?
        .json::<ExperimentListResponse>()
        .await
        .context("deserialize experiment list response")?;
        assert!(
            listed.runs.is_empty(),
            "rejected target sessions must not persist an experiment run: {:?}",
            listed.runs
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

fn run_request_with_target(
    tenant_id: TenantId,
    name: &str,
    target: Value,
    variant: Value,
) -> ExperimentRunRequest {
    ExperimentRunRequest {
        tenant_id,
        name: name.to_string(),
        plan_revision_uid: None,
        target: Some(target),
        variant: Some(variant),
        scorecard: Some(fixture_scorecard()),
        score_run_id: None,
        idempotency_key: Some(format!("{name}-{}", Uuid::now_v7())),
        agent_revision_variants: Vec::new(),
        release_evaluation: None,
    }
}

async fn activate_support_skill_fixture(storage_partition_id: &StoragePartitionId) -> Result<()> {
    let pool = sqlx::PgPool::connect(&test_database_url())
        .await
        .context("connect for approved skill fixture")?;
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(
            Uuid::parse_str(storage_partition_id.as_str())
                .context("storage partition id is tenant uuid")?,
        ),
    };
    let stored = create_skill_draft_fixture(
        &ArtifactRegistry::new(pool.clone()),
        &scope,
        decoded_support_skill_package()?,
    )
    .await?;
    moa_artifacts::test_fixtures::activate_revision(
        &pool,
        TenantScope::from_action_rule_scope(&scope)?,
        moa_artifacts::release::ActivationTarget::SkillVisibility {
            artifact_uid: stored.artifact_uid,
        },
        stored.revision_uid,
    )
    .await
    .context("activate approved learned skill fixture")?;
    Ok(())
}

async fn create_skill_draft_fixture(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    package: SkillPackage,
) -> Result<moa_artifacts::registry::StoredArtifactRevision> {
    let package = package.validate()?;
    let document = skill_artifact_document_from_package(&package, ArtifactStatus::Draft)?;
    let source_text = if let Some(file) = package
        .files
        .iter()
        .find(|file| file.path == SKILL_ARTIFACT_PATH)
    {
        file.content.clone()
    } else {
        document.to_yaml()?.into_bytes()
    };
    let files = package
        .files
        .iter()
        .map(|file| NewArtifactFile {
            path: file.path.clone(),
            content: file.content.clone(),
            content_type: file.content_type.clone(),
            executable: file.executable,
        })
        .collect::<Vec<_>>();
    registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: &source_text,
                files: &files,
            },
        )
        .await
        .context("create learned skill fixture draft")
}

fn decoded_support_skill_package() -> Result<SkillPackage> {
    let mut files = Vec::new();
    for file in support_skill_package().files {
        let content = BASE64
            .decode(&file.content_base64)
            .context("decode support skill fixture file")?;
        let mut package_file = SkillPackageFile::new(file.path, content);
        if let Some(content_type) = file.content_type {
            package_file = package_file.with_content_type(content_type);
        }
        package_file = package_file.with_executable(file.executable);
        files.push(package_file);
    }
    Ok(SkillPackage::new(files))
}

async fn run_agent_loop_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    agent_revision_uid: Uuid,
) -> Result<ExperimentRunResponse> {
    let request = ExperimentRunRequest {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
        name: "spilled-order-support-agent-loop".to_string(),
        plan_revision_uid: None,
        target: Some(json!({
            "kind": "agent_loop",
            "prompt": "A customer says soup spilled across the delivery bag. They have a clear photo and ask whether we can replace the order.",
            "session_id": null,
            "agent": { "revision_uid": agent_revision_uid },
            "model": "scripted-loadtest",
            "attachments": []
        })),
        variant: Some(json!({
            "name": "delivery-support-skill",
            "model": "scripted-loadtest",
            "artifact_revision_uids": [],
            "skill_refs": ["skill://delivery-support"],
            "execution_template": null,
            "metadata": { "lane": "deterministic-e2e" }
        })),
        scorecard: Some(fixture_scorecard()),
        score_run_id: None,
        idempotency_key: Some(format!("agent-loop-{}", Uuid::now_v7())),
        agent_revision_variants: Vec::new(),
        release_evaluation: None,
    };
    post_json_with_identity(client, ingress, "Experiments", "run", identity, &request)
        .await?
        .json::<ExperimentRunResponse>()
        .await
        .context("deserialize experiment run response")
}

async fn import_and_validate_agent_candidate(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    source_text: &str,
) -> Result<ArtifactPublishResponse> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
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
    let validated = post_json_with_identity(
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
    assert_eq!(validated.status, "draft");
    assert_validation_report_has_no_errors(&validated.validation_report)?;
    Ok(validated)
}

async fn wait_for_experiment_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    run_uid: Uuid,
) -> Result<ExperimentRunStatusResponse> {
    let request = ExperimentRunStatusRequest {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
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
        if status.session_id.is_some() && matches!(status.status.as_str(), "completed") {
            return Ok(status);
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for experiment {run_uid} to link a session and finish; last status: {last_status:?}"
    )
}

async fn wait_for_brain_response_text(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: moa_core::types::identifiers::SessionId,
    expected_text: &str,
) -> Result<Vec<EventRecord>> {
    let mut last_events = Vec::new();
    for _attempt in 0..90 {
        let events = fetch_events(client, ingress, identity, session_id).await?;
        if events.iter().any(|record| {
            matches!(&record.event, Event::BrainResponse { text, .. } if text == expected_text)
        }) {
            return Ok(events);
        }
        last_events = events;
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for BrainResponse `{expected_text}` in session {session_id}; observed events: {}",
        summarize_events(&last_events)
    )
}

async fn fetch_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: moa_core::types::identifiers::SessionId,
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
    format!(
        "{}/restate/call/{service}/{handler}",
        ingress.trim_end_matches('/')
    )
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
                && input.get("path").and_then(serde_json::Value::as_str)
                    == Some(SUPPORT_SKILL_PATH) =>
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

fn write_scripted_fixture(path: &Path, final_text: &str) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "keyed": [{
            "match": "You classify one user turn into MOA's public execution decision.",
            "completion": {
                "content": r#"{"label":"execute","strategy":"inline","rationale":"The turn requires bounded execution.","confidence_bps":9500,"missing_inputs":[]}"#,
                "tool_calls": []
            }
        }],
        "responses": [
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
                    "content": final_text,
                    "tool_calls": []
                }
            }
        ]
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted fixture")?;
    fs::write(path, body).context("write scripted fixture")
}

fn support_skill_package() -> SkillPackageDocument {
    let skill_md = r#"---
name: delivery-support
description: "Resolve damaged or spilled food delivery support requests from clear customer evidence."
metadata:
  moa-tags: "support,delivery,refund,replacement,food"
---

# Delivery Support

Use this when a customer reports a delivery that arrived spilled, crushed, leaking, missing items, unsafe, or otherwise damaged.

When there is a clear customer photo and an order id, recommend a replacement or refund review. Ask for clearer evidence only when the description or image is ambiguous.
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
        source_uri: Some("test://experiment-agent-loop/delivery-support".to_string()),
        metadata: json!({}),
    }
}

fn support_agent_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: delivery-support-agent
  description: Test support agent for deterministic experiment E2E coverage.
status: draft
definition:
  type: agent
  spec:
    display_name: Delivery Support Agent
    purpose:
      summary: Resolve damaged or spilled delivery support requests.
      default_task: Read the delivery support skill and recommend the next support action.
      expected_outputs:
        - support next step
    instruction_policy:
      system_prompt: You are a delivery support agent. Use available support instructions before giving a resolution.
    tool_policy:
      mode: allowlist
      tools:
        - file_read
"#
}

/// The minimal runnable scorecard: one deterministic blocking requirement.
fn fixture_scorecard() -> ExperimentScorecard {
    ExperimentScorecard::new(vec![ScorecardRequirement {
        evaluator_id: "target_completed".to_string(),
        evaluator_version: "v1".to_string(),
        config: json!({}),
        effect: ScorecardEffect::Blocking,
    }])
    .expect("fixture scorecard is valid")
}
