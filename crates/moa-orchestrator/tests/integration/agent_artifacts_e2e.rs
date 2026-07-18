//! End-to-end coverage for artifact-backed skills through Restate.

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
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::skill::SkillActionKind;
use moa_core::traits::Identity;
use moa_core::wire::skills::{
    SkillImportRequest, SkillImportResponse, SkillPackageDocument, SkillPackageDocumentFile,
};
use moa_core::{
    events::Event, types::action_policy::ActionRuleScope, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::session::SessionStatus,
};
use moa_skills::artifact::skill_definition_from_package;
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
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
fn refund_skill_fixture_is_instruction_action_skill_without_execution_plan() -> Result<()> {
    // Pins: an imported skill may combine instructions and a governed action without requiring
    // an execution-plan template.
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
    let definition = skill_definition_from_package(&package)?;

    assert_eq!(package.name, "refund-triage");
    assert_eq!(definition.instructions.path, "SKILL.md");
    assert_eq!(definition.actions.len(), 1);
    assert_eq!(definition.actions[0].id, "classify_refund");
    assert_eq!(definition.actions[0].kind, SkillActionKind::ConnectorAction);
    assert_eq!(
        definition.actions[0].artifact_ref,
        Some(ArtifactRef::action("orders", "classify_refund"))
    );
    assert_eq!(package.manifest.allowed_tools, vec!["file_read"]);
    assert_eq!(definition.allowed_tools, vec!["file_read"]);
    assert!(definition.execution_plan.is_none());
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
async fn support_agent_selects_refund_skill_without_starting_execution_run() -> Result<()> {
    // Pins: selecting a custom instruction/action skill in Execute/Inline materializes its
    // instructions but does not implicitly start a detached execution run.
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
        assert_eq!(
            skill_file_read_counts(&events),
            (1, 1),
            "expected exactly one call and one successful result for the selected refund skill package; observed events: {}",
            detailed_event_summary(&events)
        );
        assert_eq!(
            events
                .iter()
                .filter(|record| matches!(&record.event, Event::ExecutionRunStarted(_)))
                .count(),
            0,
            "skill selection alone must not start an execution run"
        );

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

fn tenant_scope(storage_partition_id: &StoragePartitionId) -> Result<ActionRuleScope> {
    let tenant_id = tenant_id_from_storage_partition_id(storage_partition_id);
    Ok(ActionRuleScope::Tenant { tenant_id })
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
    meta: &moa_core::types::session::SessionMeta,
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
    format!(
        "{}/restate/call/{service}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn object_url(ingress: &str, service: &str, object_id: SessionId, handler: &str) -> String {
    format!(
        "{}/restate/call/{service}/{object_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn skill_file_read_counts(events: &[EventRecord]) -> (usize, usize) {
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

    let successful_results = events
        .iter()
        .filter(|record| {
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
        .count();
    (read_tool_ids.len(), successful_results)
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
        "keyed": [{
            "match": "You classify one user turn into MOA's public execution decision.",
            "completion": {
                "content": json!({
                    "label": "execute",
                    "reason": "bounded_interactive_work",
                    "confidence_bps": 10_000,
                    "missing_inputs": []
                }).to_string(),
                "tool_calls": []
            }
        }],
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
