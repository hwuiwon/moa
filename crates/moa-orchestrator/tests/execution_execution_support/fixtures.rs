//! Thin scenario composition over the production Session, Artifact, Policy, and Execution APIs.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::document::ArtifactKind;
use moa_artifacts::reference::ArtifactRef;
use moa_core::events::Event;
use moa_core::types::action_policy::{ActionPolicyEffect, ActionRuleScope};
use moa_core::types::events_stream::{EventRange, EventRecord};
use moa_core::types::execution_planning::{ExecutionMode, ExecutionRouteReason};
use moa_core::types::execution_planning::{ExecutionRunStarted, ExecutionTemplateInvocation};
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_core::types::session::SessionStatus;
use moa_core::wire::artifacts::{
    ArtifactFileDocument, ArtifactImportRequest, ArtifactImportResponse, ArtifactPublishRequest,
    ArtifactPublishResponse,
};
use moa_core::wire::turn::{
    SessionProgress, SessionProgressRequest, StartTurnRequest, TurnOutcome,
};
use moa_execution::wire::{
    ExecutionRunRequest, ExecutionStatusResponse, ExecutionTaskListRequest,
    ExecutionTaskListResponse,
};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_test_support::{IsolatedTest, OrchestratorTestFixture, TestApiClient};
use serde_json::{Value, json};
use tokio::time::Instant;

/// Maximum wait for one local service scenario transition.
pub(crate) const SERVICE_TIMEOUT: Duration = Duration::from_secs(90);
/// Poll interval for local service state that has no push notification.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Stable system-prompt fragment used to target the pre-mode classifier request.
pub(crate) const ROUTE_CLASSIFIER_MATCH: &str =
    "You classify one user turn into MOA's execution mode.";

/// Builds one strict scripted response for the production route classifier.
pub(crate) fn route_classifier_completion(
    mode: ExecutionMode,
    reason: ExecutionRouteReason,
) -> Value {
    let label = match mode {
        ExecutionMode::Respond => "respond",
        ExecutionMode::Act => "act",
        ExecutionMode::Run => "run",
    };
    json!({
        "match": ROUTE_CLASSIFIER_MATCH,
        "completion": {
            "content": json!({
                "label": label,
                "reason": reason,
                "confidence_bps": 10_000,
                "missing_inputs": []
            }).to_string(),
            "tool_calls": []
        }
    })
}

/// Builds one strict scripted clarification response for the production route classifier.
pub(crate) fn route_classifier_needs_input_completion(
    reason: ExecutionRouteReason,
    missing_inputs: &[&str],
) -> Value {
    json!({
        "match": ROUTE_CLASSIFIER_MATCH,
        "completion": {
            "content": json!({
                "label": "needs_input",
                "reason": reason,
                "confidence_bps": 10_000,
                "missing_inputs": missing_inputs
            }).to_string(),
            "tool_calls": []
        }
    })
}

/// One newly created session and its immediately started root turn.
pub(crate) struct StartedTurn {
    /// Isolated session identifier.
    pub(crate) session_id: SessionId,
    /// Owning fixture tenant.
    pub(crate) tenant_id: TenantId,
    /// Root turn workflow identifier.
    pub(crate) turn_id: String,
}

/// One exact published skill revision available to planning.
pub(crate) struct PublishedSkill {
    /// Canonical `skill://` reference.
    pub(crate) skill_ref: String,
    /// Exact published revision identifier.
    pub(crate) revision_uid: uuid::Uuid,
}

/// Creates a real session and starts one root turn through `Session/start_turn`.
pub(crate) async fn start_turn(
    test: &IsolatedTest<'_>,
    label: &str,
    objective: &str,
    execution_template: Option<ExecutionTemplateInvocation>,
) -> Result<StartedTurn> {
    let session_id = test.create_session(label).await?;
    start_turn_in_session(test, session_id, objective, execution_template).await
}

/// Starts one root turn in an already-created session after scenario-specific setup.
pub(crate) async fn start_turn_in_session(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    objective: &str,
    execution_template: Option<ExecutionTemplateInvocation>,
) -> Result<StartedTurn> {
    let session = test.client().get_session(session_id).await?;
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                user_message: objective.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                execution_template,
            },
            None,
        )
        .await
        .context("start deterministic execution scenario turn")?;
    if started.queued {
        bail!("new isolated session unexpectedly queued its first turn");
    }
    let turn_id = started
        .turn_id
        .context("new isolated session did not return a turn id")?;
    Ok(StartedTurn {
        session_id,
        tenant_id: session.tenant_id,
        turn_id,
    })
}

/// Waits for one exact root turn outcome through the fixture's bounded poller.
pub(crate) async fn await_turn_outcome(
    client: &TestApiClient,
    started: &StartedTurn,
) -> Result<TurnOutcome> {
    client
        .session(started.session_id.to_string())
        .await_turn_outcome(&started.turn_id, SERVICE_TIMEOUT, POLL_INTERVAL)
        .await
        .context("await deterministic root-turn outcome")
}

/// Waits until the full user message, including detached-run synthesis, settles.
pub(crate) async fn await_session_settled(
    client: &TestApiClient,
    session_id: SessionId,
) -> Result<SessionStatus> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let status = client
            .session(session_id.to_string())
            .status()
            .await
            .context("poll Session/status")?;
        if !matches!(status, SessionStatus::Created | SessionStatus::Running) {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!(
                "session {session_id} did not settle within {SERVICE_TIMEOUT:?}; last status: {status:?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Reads the unredacted durable session event log from the session store service.
pub(crate) async fn raw_events(
    client: &TestApiClient,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    client
        .get_events(session_id, EventRange::all())
        .await
        .context("read raw durable session events")
}

/// Waits for the exact admitted-run event for one run UID.
pub(crate) async fn await_run_started_event(
    client: &TestApiClient,
    session_id: SessionId,
    run_uid: uuid::Uuid,
) -> Result<ExecutionRunStarted> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let events = raw_events(client, session_id).await?;
        if let Some(started) = events.iter().find_map(|record| match &record.event {
            Event::ExecutionRunStarted(started) if started.run_uid == run_uid => {
                Some(started.clone())
            }
            _ => None,
        }) {
            return Ok(started);
        }
        if Instant::now() >= deadline {
            bail!(
                "run {run_uid} did not publish ExecutionRunStarted within {SERVICE_TIMEOUT:?}; events: {}",
                event_summary(&events)
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Builds the parent-authorized request shared by Execution read APIs.
pub(crate) fn execution_run_request(
    started: &StartedTurn,
    run_uid: uuid::Uuid,
) -> ExecutionRunRequest {
    ExecutionRunRequest {
        tenant_id: started.tenant_id,
        contact_id: None,
        session_id: started.session_id,
        run_uid,
    }
}

/// Waits for one execution run to reach a terminal database projection.
pub(crate) async fn await_execution_terminal(
    client: &TestApiClient,
    request: &ExecutionRunRequest,
) -> Result<ExecutionStatusResponse> {
    await_execution_terminal_with_timeout(client, request, SERVICE_TIMEOUT).await
}

/// Waits for one execution run to reach terminal state within an explicit scenario bound.
pub(crate) async fn await_execution_terminal_with_timeout(
    client: &TestApiClient,
    request: &ExecutionRunRequest,
    timeout: Duration,
) -> Result<ExecutionStatusResponse> {
    let deadline = Instant::now() + timeout;
    loop {
        let status: ExecutionStatusResponse = client
            .post_call("/Execution/status", request)
            .await
            .context("poll Execution/status")?;
        if status.run.status.is_terminal() {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!(
                "execution run {} did not become terminal within {timeout:?}; last status: {:?}",
                request.run_uid,
                status.run.status
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Waits until Session/progress exposes the requested run as active and incomplete.
pub(crate) async fn await_active_execution_progress(
    client: &TestApiClient,
    request: &ExecutionRunRequest,
) -> Result<moa_core::events::ExecutionProgress> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let progress: SessionProgress = client
            .post_call(
                &format!("/Session/{}/progress", request.session_id),
                &SessionProgressRequest {
                    event_range: EventRange::recent(50),
                },
            )
            .await
            .context("poll Session/progress for active execution")?;
        if let Some(active) = progress
            .active_execution_progress
            .iter()
            .find(|active| {
                active.run_uid == request.run_uid
                    && active.total > 0
                    && active.completed < active.total
            })
            .cloned()
        {
            return Ok(active);
        }
        if Instant::now() >= deadline {
            bail!(
                "execution run {} never appeared as active and incomplete within {SERVICE_TIMEOUT:?}; last active progress: {:?}",
                request.run_uid,
                progress.active_execution_progress
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Loads the first bounded page of task projections for a small scenario run.
pub(crate) async fn list_execution_tasks(
    client: &TestApiClient,
    request: ExecutionRunRequest,
) -> Result<ExecutionTaskListResponse> {
    client
        .post_call(
            "/Execution/list_tasks",
            &ExecutionTaskListRequest {
                run: request,
                limit: Some(100),
                cursor: None,
            },
        )
        .await
        .context("list execution task projections")
}

/// Imports and publishes one real skill artifact plus its exact `SKILL.md` package file.
pub(crate) async fn publish_skill(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    tenant_id: TenantId,
    name: &str,
    source_text: String,
    skill_markdown: &str,
) -> Result<PublishedSkill> {
    fixture
        .grant_default_tenant_admin(tenant_id)
        .await
        .context("grant tenant admin before artifact import")?;
    let scope = ActionRuleScope::Tenant { tenant_id };
    let imported: ArtifactImportResponse = client
        .post_call(
            "/Artifacts/import",
            &ArtifactImportRequest {
                scope,
                source_format: "yaml".to_string(),
                source_text,
                files: vec![ArtifactFileDocument {
                    path: "SKILL.md".to_string(),
                    content_base64: BASE64.encode(skill_markdown.as_bytes()),
                    content_type: Some("text/markdown; charset=utf-8".to_string()),
                    executable: false,
                }],
            },
        )
        .await
        .context("import deterministic skill artifact")?;
    if imported.status != "draft" {
        bail!("artifact import returned non-draft status: {imported:?}");
    }
    assert_validation_report_has_no_errors(&imported.validation_report)?;

    let published: ArtifactPublishResponse = client
        .post_call(
            "/Artifacts/publish",
            &ArtifactPublishRequest {
                scope,
                revision_uid: imported.revision_uid,
            },
        )
        .await
        .context("publish deterministic skill artifact")?;
    if published.status != "published" {
        bail!("artifact publish returned non-published status: {published:?}");
    }
    assert_validation_report_has_no_errors(&published.validation_report)?;

    Ok(PublishedSkill {
        skill_ref: ArtifactRef::artifact(ArtifactKind::Skill, name).to_string(),
        revision_uid: published.revision_uid,
    })
}

/// Grants tenant admin and upserts one exact tool-scoped Allow rule through production policy.
pub(crate) async fn seed_allow_policy(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    tenant_id: TenantId,
    tool_name: &str,
) -> Result<()> {
    fixture
        .grant_default_tenant_admin(tenant_id)
        .await
        .context("grant tenant admin before action-policy upsert")?;
    client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &UpsertActionPolicyRuleRequest {
                tenant_id,
                contact_id: None,
                tool_name: tool_name.to_string(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("deterministic execution service fixture".to_string()),
            },
        )
        .await
        .context("upsert exact fixture-tool Allow rule")
}

fn assert_validation_report_has_no_errors(report: &Value) -> Result<()> {
    let Some(errors) = report.get("errors").and_then(Value::as_array) else {
        bail!("artifact validation report omitted errors array: {report}");
    };
    if errors.is_empty() {
        return Ok(());
    }
    bail!("artifact validation failed: {errors:?}")
}

fn event_summary(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| format!("#{} {:?}", record.sequence_num, record.event_type))
        .collect::<Vec<_>>()
        .join(", ")
}
