//! End-to-end coverage for artifact-backed skills and workflows through Restate.

use std::collections::HashSet;
use std::time::Duration;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::validation::validate_for_status;
use moa_core::traits::Identity;
use moa_core::wire::artifacts::{
    ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest, ArtifactPublishResponse,
};
use moa_core::wire::skills::{
    SkillImportRequest, SkillImportResponse, SkillPackageDocument, SkillPackageDocumentFile,
};
use moa_core::wire::workflows::{
    WorkflowRunRequest, WorkflowRunResponse, WorkflowRunStatus, WorkflowStatusRequest,
};
use moa_core::{
    ActionRuleScope, Event, EventRange, EventRecord, ModelId, SessionId, SessionStatus,
    StoragePartitionId, TenantId,
};
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_test_support::postgres::test_database_url;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_admin, register_deployment, reserve_orchestrator_ports, restate_admin_url,
    restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, storage_partition_id_from_meta, test_session_meta,
    user_message,
};

const REFUND_SKILL_PATH: &str = ".moa/skills/refund-triage/SKILL.md";
const REFUND_SKILL_PROVIDER_ID: &str = "read_refund_triage_skill";

#[test]
fn damaged_food_workflow_fixture_is_publishable() -> Result<()> {
    // Pins: the e2e workflow fixture remains a valid publishable workflow artifact.
    let document = ArtifactDocument::from_yaml(damaged_food_workflow_source())
        .context("parse damaged food workflow fixture")?;
    let report = validate_for_status(&document, ArtifactStatus::Published);

    assert!(
        report.is_ok(),
        "workflow fixture should publish cleanly: {report:?}"
    );
    assert_eq!(document.metadata.name, "damaged-food-replacement");
    Ok(())
}

#[test]
fn refund_skill_fixture_exposes_linkable_action_metadata() -> Result<()> {
    // Pins: the e2e skill fixture keeps UI-authored actions available to skill selection.
    let document = refund_skill_package();
    let mut files = Vec::new();
    for file in document.files {
        let content = BASE64
            .decode(&file.content_base64)
            .context("decode fixture skill package file")?;
        let mut package_file = SkillPackageFile::new(file.path, content);
        if let Some(content_type) = file.content_type {
            package_file = package_file.with_content_type(content_type);
        }
        package_file = package_file.with_executable(file.executable);
        files.push(package_file);
    }
    let package = SkillPackage::new(files).validate()?;

    assert_eq!(package.name, "refund-triage");
    assert_eq!(package.manifest.actions.len(), 1);
    assert_eq!(package.manifest.actions[0].id, "classify_refund");
    assert_eq!(package.manifest.allowed_tools, vec!["file_read"]);
    Ok(())
}

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    provider_override_fixture: Option<&Path>,
    log_path: &Path,
) -> Result<Child> {
    let log_file = fs::File::create(log_path).context("create orchestrator e2e log")?;
    let log_file_for_stderr = log_file
        .try_clone()
        .context("clone orchestrator e2e log handle")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"));
    command
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
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_for_stderr));
    if let Some(path) = provider_override_fixture {
        command
            .env(
                "MOA_PROVIDERS_OVERRIDE",
                format!("scripted:{}", path.display()),
            )
            .env_remove("MOA_ANTHROPIC_API_KEY")
            .env_remove("MOA_OPENAI_API_KEY")
            .env_remove("MOA_GOOGLE_API_KEY");
    }

    command
        .spawn()
        .context("spawn moa-orchestrator binary for artifact e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn support_agent_selects_refund_skill_from_customer_message() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("skill-selection-script.json");
    let final_text = "I checked the refund triage runbook. The damaged food order qualifies for a replacement or refund after verifying the photo and order id.";
    write_skill_file_read_fixture(&fixture_path, final_text)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut identity = test_user_identity();
    let mut meta = test_session_meta(&format!("agent-artifacts-skill-{}", Uuid::now_v7()));
    meta.model = ModelId::new("scripted-loadtest");
    let storage_partition_id = storage_partition_id_from_meta(&meta);
    identity.tenant_id = meta.tenant_id;
    grant_tenant_admin(&identity, &storage_partition_id).await?;
    let orchestrator_log = memory_dir.path().join("orchestrator.log");
    let mut orchestrator = spawn_orchestrator(
        ports,
        &memory_dir,
        &sandbox_dir,
        Some(&fixture_path),
        &orchestrator_log,
    )?;

    let result = async {
        wait_for_orchestrator_live(&client, ports.health, &mut orchestrator, &orchestrator_log)
            .await?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_refund_skill(&client, ingress, &identity, &storage_partition_id).await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;

        let prompt = "A customer says their ramen order arrived spilled all over the bag. \
            They uploaded a clear photo and want a refund or replacement. Can you handle this?";
        post_user_message(&client, ingress, &identity, session_id, prompt).await?;

        let events =
            wait_for_brain_response_text(&client, ingress, &identity, session_id, final_text)
                .await?;
        wait_for_status(&client, ingress, &identity, session_id, SessionStatus::Paused).await?;
        assert!(
            saw_successful_skill_file_read(&events),
            "expected the agent loop to read the selected refund skill package; observed events: {}",
            detailed_event_summary(&events)
        );
        assert!(
            !saw_workflow_tool_call(&events),
            "skill-only support conversation should not invoke a workflow tool"
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
async fn damaged_food_workflow_run_starts_from_published_artifact() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("damaged-food-workflow-script.json");
    write_scripted_text_fixture(
        &fixture_path,
        "I checked the damaged food report and queued it for approval.",
    )?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut identity = test_user_identity();
    let mut meta = test_session_meta(&format!("agent-artifacts-workflow-{}", Uuid::now_v7()));
    meta.model = ModelId::new("scripted-loadtest");
    let storage_partition_id = storage_partition_id_from_meta(&meta);
    identity.tenant_id = meta.tenant_id;
    grant_tenant_admin(&identity, &storage_partition_id).await?;
    let orchestrator_log = memory_dir.path().join("orchestrator.log");
    let mut orchestrator = spawn_orchestrator(
        ports,
        &memory_dir,
        &sandbox_dir,
        Some(&fixture_path),
        &orchestrator_log,
    )?;

    let result = async {
        wait_for_orchestrator_live(&client, ports.health, &mut orchestrator, &orchestrator_log)
            .await?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_and_publish_damaged_food_workflow(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
        )
        .await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;

        let response = start_damaged_food_workflow(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            Some(session_id),
            "ORD-4821",
        )
        .await?;
        assert_eq!(response.status, "queued");

        let status = wait_for_workflow_status(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            response.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(status.session_id, Some(session_id));
        assert_eq!(status.current_node_id.as_deref(), Some("review_resolution"));
        assert_eq!(
            node_ids(&status),
            vec![
                "start",
                "verify_evidence",
                "choose_resolution",
                "review_resolution"
            ]
        );
        assert_eq!(status.node_runs[3].status, "pending_review");
        assert!(
            status.error.is_none(),
            "workflow should pause cleanly: {status:?}"
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
async fn workflow_association_and_skill_selection_share_one_support_session() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("mixed-workflow-skill-script.json");
    let final_text = "I used the refund triage runbook and kept the damaged-food workflow attached to this session for tracking.";
    write_skill_file_read_fixture(&fixture_path, final_text)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut identity = test_user_identity();
    let mut meta = test_session_meta(&format!("agent-artifacts-mixed-{}", Uuid::now_v7()));
    meta.model = ModelId::new("scripted-loadtest");
    let storage_partition_id = storage_partition_id_from_meta(&meta);
    identity.tenant_id = meta.tenant_id;
    grant_tenant_admin(&identity, &storage_partition_id).await?;
    let orchestrator_log = memory_dir.path().join("orchestrator.log");
    let mut orchestrator = spawn_orchestrator(
        ports,
        &memory_dir,
        &sandbox_dir,
        Some(&fixture_path),
        &orchestrator_log,
    )?;

    let result = async {
        wait_for_orchestrator_live(&client, ports.health, &mut orchestrator, &orchestrator_log)
            .await?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_refund_skill(&client, ingress, &identity, &storage_partition_id).await?;
        import_and_publish_damaged_food_workflow(&client, ingress, &identity, &storage_partition_id)
            .await?;
        let session_id = create_session(&client, ingress, &identity, &meta).await?;

        let prompt = "Use our refund triage guidance while tracking the damaged-food workflow \
            for order ORD-7002. The customer says sauce leaked through the bag and wants a credit.";
        post_user_message(&client, ingress, &identity, session_id, prompt).await?;

        let events =
            wait_for_brain_response_text(&client, ingress, &identity, session_id, final_text)
                .await?;
        wait_for_status(&client, ingress, &identity, session_id, SessionStatus::Paused).await?;
        assert!(
            saw_successful_skill_file_read(&events),
            "mixed session should still materialize and read the selected refund skill; observed events: {}",
            detailed_event_summary(&events)
        );
        assert!(
            !saw_workflow_tool_call(&events),
            "workflow association should not make the agent loop invent workflow tool calls"
        );

        let workflow_run = start_damaged_food_workflow(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            Some(session_id),
            "ORD-7002",
        )
        .await?;
        assert_eq!(workflow_run.status, "queued");

        let status = wait_for_workflow_status(
            &client,
            ingress,
            &identity,
            &storage_partition_id,
            workflow_run.run_id,
            "pending_review",
        )
        .await?;
        assert_eq!(status.session_id, Some(session_id));
        assert_eq!(status.current_node_id.as_deref(), Some("review_resolution"));
        assert_eq!(
            node_ids(&status),
            vec![
                "start",
                "verify_evidence",
                "choose_resolution",
                "review_resolution"
            ]
        );
        assert_eq!(status.node_runs[3].status, "pending_review");
        assert!(status.error.is_none(), "workflow should pause cleanly: {status:?}");

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn import_refund_skill(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
) -> Result<()> {
    let request = SkillImportRequest {
        scope: tenant_scope(storage_partition_id)?,
        packages: vec![refund_skill_package()],
    };
    let import = post_json_with_identity(client, ingress, "Skills", "import", identity, &request)
        .await?
        .json::<SkillImportResponse>()
        .await
        .context("deserialize skill import response")?;
    assert_eq!(import.imported, 1);
    Ok(())
}

async fn import_and_publish_damaged_food_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
) -> Result<ArtifactPublishResponse> {
    let scope = tenant_scope(storage_partition_id)?;
    let import_request = ArtifactImportRequest {
        scope,
        source_format: "yaml".to_string(),
        source_text: damaged_food_workflow_source().to_string(),
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

fn tenant_scope(storage_partition_id: &StoragePartitionId) -> Result<ActionRuleScope> {
    let tenant_id = tenant_id_from_workspace(storage_partition_id)?;
    Ok(ActionRuleScope::Tenant { tenant_id })
}

fn tenant_id_from_workspace(storage_partition_id: &StoragePartitionId) -> Result<TenantId> {
    Uuid::parse_str(storage_partition_id.as_str())
        .map(TenantId::from)
        .context("artifact e2e storage partition id should be a tenant UUID")
}

async fn start_damaged_food_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    session_id: Option<SessionId>,
    order_id: &str,
) -> Result<WorkflowRunResponse> {
    let request = WorkflowRunRequest {
        tenant_id: tenant_id_from_workspace(storage_partition_id)?,
        workflow_ref: "workflow://damaged-food-replacement".to_string(),
        input: json!({
            "order_id": order_id,
            "damage_summary": "clear photo shows sauce leaked through the delivery bag",
            "customer_requested": "refund_or_replacement"
        }),
        session_id,
        idempotency_key: Some(format!("workflow-{order_id}")),
    };
    post_json_with_identity(client, ingress, "Workflows", "run", identity, &request)
        .await?
        .json::<WorkflowRunResponse>()
        .await
        .context("deserialize workflow run response")
}

async fn wait_for_workflow_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    storage_partition_id: &StoragePartitionId,
    run_id: Uuid,
    expected: &str,
) -> Result<WorkflowRunStatus> {
    let request = WorkflowStatusRequest {
        tenant_id: tenant_id_from_workspace(storage_partition_id)?,
        run_id,
    };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Workflows", "status", identity, &request)
                .await?
                .json::<WorkflowRunStatus>()
                .await
                .context("deserialize workflow status response")?;
        if status.status == expected {
            return Ok(status);
        }
        if status.status == "failed" {
            bail!("workflow run failed before reaching {expected}: {status:?}");
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for workflow run {run_id} to reach {expected}; last status: {last_status:?}"
    )
}

async fn wait_for_orchestrator_live(
    client: &reqwest::Client,
    health_port: u16,
    child: &mut Child,
    log_path: &Path,
) -> Result<()> {
    let url = format!("http://127.0.0.1:{health_port}/_health/live");
    let mut last_observation = String::from("not probed");
    for _attempt in 0..60 {
        if let Some(status) = child.try_wait().context("poll spawned orchestrator")? {
            bail!(
                "spawned orchestrator exited before health check passed: {status}\n{}",
                orchestrator_log_tail(log_path)
            );
        }
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_observation = format!("HTTP {}", response.status());
            }
            Err(error) => {
                last_observation = error.to_string();
            }
        }
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for spawned orchestrator health at {url}: {last_observation}\n{}",
        orchestrator_log_tail(log_path)
    )
}

fn orchestrator_log_tail(log_path: &Path) -> String {
    let contents = match fs::read_to_string(log_path) {
        Ok(contents) => contents,
        Err(error) => {
            return format!(
                "failed to read orchestrator log {}: {error}",
                log_path.display()
            );
        }
    };
    if contents.trim().is_empty() {
        return format!("orchestrator log {} was empty", log_path.display());
    }
    let mut lines = contents.lines().rev().take(80).collect::<Vec<_>>();
    lines.reverse();
    format!(
        "orchestrator log tail from {}:\n{}",
        log_path.display(),
        lines.join("\n")
    )
}

async fn create_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    meta: &moa_core::SessionMeta,
) -> Result<SessionId> {
    let create_request = client.post(service_url(ingress, "SessionStore", "create_session"));
    let session_id = with_identity(create_request, identity)
        .json(meta)
        .send()
        .await
        .context("create session via Restate ingress")?
        .error_for_status()
        .context("create_session should succeed")?
        .json::<SessionId>()
        .await
        .context("deserialize create_session response")?;
    grant_session_participant(identity, session_id).await?;

    client
        .post(service_url(ingress, "SessionStore", "init_session_vo"))
        .json(&init_session_vo_request(session_id, meta.clone()))
        .send()
        .await
        .context("initialize session VO state")?
        .error_for_status()
        .context("init_session_vo should succeed")?;

    Ok(session_id)
}

async fn post_user_message(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    prompt: &str,
) -> Result<()> {
    let post_message = client.post(object_url(ingress, "Session", session_id, "post_message"));
    with_identity(post_message, identity)
        .json(&user_message(prompt))
        .send()
        .await
        .context("call Session/post_message")?
        .error_for_status()
        .context("post_message should succeed")?;
    Ok(())
}

async fn wait_for_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<SessionStatus> {
    for _attempt in 0..60 {
        let request = client.post(object_url(ingress, "Session", session_id, "status"));
        let status = with_identity(request, identity)
            .send()
            .await
            .context("call Session/status")?
            .error_for_status()
            .context("status should succeed")?
            .json::<SessionStatus>()
            .await
            .context("deserialize status response")?;
        if status == expected {
            return Ok(status);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for status {expected:?} for session {session_id}")
}

async fn wait_for_brain_response_text(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
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
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    let request = client.post(service_url(ingress, "SessionStore", "get_events"));
    with_identity(request, identity)
        .json(&get_events_request(session_id, EventRange::all()))
        .send()
        .await
        .context("fetch events via Restate ingress")?
        .error_for_status()
        .context("get_events should succeed")?
        .json::<Vec<EventRecord>>()
        .await
        .context("deserialize event response")
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

fn object_url(ingress: &str, service: &str, object_id: SessionId, handler: &str) -> String {
    format!(
        "{}/{service}/{object_id}/{handler}",
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
                && input.get("path").and_then(Value::as_str) == Some(REFUND_SKILL_PATH) =>
            {
                Some(*tool_id)
            }
            _ => None,
        })
        .collect::<HashSet<_>>();

    events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolResult {
                tool_id,
                output,
                success: true,
                ..
            } if read_tool_ids.contains(tool_id) && output.to_text().contains("Refund Triage")
        )
    })
}

fn saw_workflow_tool_call(events: &[EventRecord]) -> bool {
    events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolCall { tool_name, .. } if tool_name.contains("workflow")
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

fn detailed_event_summary(events: &[EventRecord]) -> String {
    if events.is_empty() {
        return "<none>".to_string();
    }

    events
        .iter()
        .map(|record| match &record.event {
            Event::ToolCall {
                tool_id,
                provider_tool_use_id,
                tool_name,
                input,
                ..
            } => format!(
                "#{} ToolCall id={tool_id} provider={provider_tool_use_id:?} name={tool_name} input={input}",
                record.sequence_num
            ),
            Event::ToolResult {
                tool_id,
                provider_tool_use_id,
                output,
                success,
                ..
            } => format!(
                "#{} ToolResult id={tool_id} provider={provider_tool_use_id:?} success={success} output={}",
                record.sequence_num,
                truncate_for_summary(&output.to_text())
            ),
            Event::ToolError {
                tool_id,
                provider_tool_use_id,
                tool_name,
                error,
                ..
            } => format!(
                "#{} ToolError id={tool_id} provider={provider_tool_use_id:?} name={tool_name} error={error}",
                record.sequence_num
            ),
            Event::BrainResponse { text, .. } => format!(
                "#{} BrainResponse text={}",
                record.sequence_num,
                truncate_for_summary(text)
            ),
            _ => format!("#{} {:?}", record.sequence_num, record.event_type),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn node_ids(status: &WorkflowRunStatus) -> Vec<&str> {
    status
        .node_runs
        .iter()
        .map(|node_run| node_run.node_id.as_str())
        .collect()
}

fn truncate_for_summary(value: &str) -> String {
    const LIMIT: usize = 500;
    if value.chars().count() <= LIMIT {
        return value.replace('\n', "\\n");
    }

    let truncated = value.chars().take(LIMIT).collect::<String>();
    format!("{}...", truncated.replace('\n', "\\n"))
}

fn write_skill_file_read_fixture(path: &Path, final_text: &str) -> Result<()> {
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
                    "content": "",
                    "tool_calls": [{
                        "name": "file_read",
                        "id": REFUND_SKILL_PROVIDER_ID,
                        "input": { "path": REFUND_SKILL_PATH }
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
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted skill fixture")?;
    fs::write(path, body).context("write scripted skill fixture")
}

fn write_scripted_text_fixture(path: &Path, final_text: &str) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": final_text,
                "tool_calls": []
            }
        }
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted text fixture")?;
    fs::write(path, body).context("write scripted text fixture")
}

fn refund_skill_package() -> SkillPackageDocument {
    let skill_md = r#"---
name: refund-triage
description: "Classify damaged food delivery complaints and decide whether refund, credit, replacement, restaurant escalation, or clearer evidence is needed."
allowed-tools: file_read
metadata:
  moa-tags: "support,refund,food-delivery,damaged-order,evidence,credit,replacement"
---

# Refund Triage

Use this when a customer reports that a food delivery arrived damaged, leaking, crushed, spilled, missing, or unsafe.

Verify the order id and evidence. A clear photo of spilled sauce, leaking packaging, crushed items, or a missing ordered item is sufficient for refund or replacement review. Ask once for clearer evidence when the photo or description is ambiguous.

If evidence is sufficient, summarize whether refund, credit, replacement, or restaurant escalation is appropriate and mention the customer-facing next step.
"#;
    let skill_yaml = r#"
inputs:
  type: object
outputs:
  type: object
connectors:
  - connector://orders
allowed_tools:
  - file_read
actions:
  - id: classify_refund
    description: Decide refund, credit, replacement, or escalation from customer evidence.
    kind: connector_action
    ref: action://orders.classify_refund
ui:
  label: Refund triage
"#;
    SkillPackageDocument {
        name: Some("refund-triage".to_string()),
        description: Some("Damaged food delivery refund triage".to_string()),
        files: vec![
            skill_file("SKILL.md", skill_md, Some("text/markdown; charset=utf-8")),
            skill_file(
                "skill.moa.yaml",
                skill_yaml,
                Some("application/yaml; charset=utf-8"),
            ),
        ],
        source_uri: Some("test://skills/refund-triage".to_string()),
        metadata: Value::Null,
    }
}

fn skill_file(
    path: impl Into<String>,
    content: &str,
    content_type: Option<&str>,
) -> SkillPackageDocumentFile {
    SkillPackageDocumentFile {
        path: path.into(),
        content_base64: BASE64.encode(content.as_bytes()),
        content_type: content_type.map(ToOwned::to_owned),
        executable: false,
    }
}

fn damaged_food_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: damaged-food-replacement
  description: Procedure for refund, credit, or replacement when food arrives damaged.
  tags:
    - support
    - food-delivery
    - refund
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
        ui:
          x: 80
          y: 120
      - id: verify_evidence
        kind: condition
        condition:
          type: exists
          path: $.damage_summary
        ui:
          x: 280
          y: 120
      - id: choose_resolution
        kind: agent
        max_turns: 2
        input:
          instruction: Decide refund, credit, replacement, or escalation after evidence review.
        ui:
          x: 520
          y: 120
      - id: review_resolution
        kind: review
        input:
          prompt: Review the proposed refund, credit, replacement, or escalation before notifying the customer.
        ui:
          x: 760
          y: 120
      - id: done
        kind: end
        ui:
          x: 1000
          y: 120
    edges:
      - id: start-to-verify
        from: start
        to: verify_evidence
      - id: verify-to-resolution
        from: verify_evidence
        to: choose_resolution
      - id: resolution-to-review
        from: choose_resolution
        to: review_resolution
      - id: review-to-done
        from: review_resolution
        to: done
"#
}

fn assert_validation_report_has_no_errors(report: &Value) -> Result<()> {
    let Some(errors) = report.get("errors").and_then(Value::as_array) else {
        bail!("validation report did not include an errors array: {report}");
    };
    if errors.is_empty() {
        return Ok(());
    }

    bail!("published workflow had validation errors: {errors:?}")
}
