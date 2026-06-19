//! End-to-end action-policy and workspace-admin review coverage.

use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{StartTurnRequest, TurnOutcomeKind};
use moa_core::{
    ActionPolicyEffect, ActionReviewDecision, ActionReviewStatus, Event, EventRange, EventRecord,
    SessionId, SessionStatus, ToolCallId, WorkspaceId,
};
use moa_orchestrator::objects::workspace::{
    WorkspaceActionPolicy, WorkspaceActionPolicyRuleInput, WorkspaceConfig,
};
use moa_orchestrator::services::action_reviews::{
    ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest,
    ListActionReviewsRequest,
};
use moa_test_support::{IsolatedTest, OrchestratorTestFixture, TestApiClient};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn action_policy_flow_covers_auto_review_decision_and_member_authz() -> Result<()> {
    // Pins: action-policy E2E scenarios share one scripted fixture process.
    let fixture = OrchestratorTestFixture::with_script(action_policy_script()).await?;
    action_policy_auto_mode_executes_shell_without_user_approval(&fixture).await?;
    admin_review_policy_records_pending_review_and_turn_continues(&fixture).await?;
    workspace_admin_clear_executes_stored_review_action(&fixture).await?;
    workspace_admin_deny_does_not_execute_stored_review_action(&fixture).await?;
    workspace_member_cannot_decide_action_review(&fixture).await?;
    Ok(())
}

async fn action_policy_auto_mode_executes_shell_without_user_approval(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: a formerly gated bash action executes under action-policy auto mode.
    let test = fixture.isolated().await;
    let session_id = test.create_session("auto-mode").await?;

    let events = run_scripted_turn(&test, session_id, "Run the auto-mode bash command.").await?;

    assert!(
        events
            .iter()
            .all(|record| !matches!(record.event, Event::ActionReviewRequested { .. })),
        "auto mode should not create an action review: {}",
        event_summary(&events)
    );
    assert_tool_result(
        &events,
        Some("auto-mode-bash"),
        "auto-mode-ok",
        true,
        "auto-mode bash result",
    );
    Ok(())
}

async fn admin_review_policy_records_pending_review_and_turn_continues(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: admin-review policy records a pending workspace action review without blocking.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-pending").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_workspace(test.client(), &meta.workspace_id).await?;
    fixture
        .grant_default_workspace_admin(&meta.workspace_id)
        .await
        .context("grant admin before adding and listing pending review")?;
    add_bash_admin_review_rule(test.client(), &meta.workspace_id).await?;

    let events = run_scripted_turn(&test, session_id, "Run the admin-review bash command.").await?;
    let review_id = action_review_id(&events).context("expected ActionReviewRequested event")?;
    let pending = list_pending_reviews(test.client(), &meta.workspace_id).await?;
    assert_eq!(pending.len(), 1, "expected exactly one pending review");
    assert_eq!(pending[0].id, review_id);
    assert_eq!(pending[0].status, ActionReviewStatus::Pending);

    assert_tool_result(
        &events,
        Some("pending-review-bash"),
        "pending workspace admin review",
        false,
        "pending-review tool result",
    );
    assert!(
        events.iter().any(
            |record| matches!(&record.event, Event::BrainResponse { text, .. } if text == "Admin review path continued.")
        ),
        "turn should continue to the final scripted response: {}",
        event_summary(&events)
    );
    let stored = test.client().get_session(session_id).await?;
    assert!(
        matches!(
            stored.status,
            SessionStatus::Paused | SessionStatus::Completed
        ),
        "admin review should not leave the session running or blocked: {:?}",
        stored.status
    );
    Ok(())
}

async fn workspace_admin_clear_executes_stored_review_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: clearing a workspace action review executes the stored request with a fresh tool id.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-clear").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_workspace(test.client(), &meta.workspace_id).await?;
    fixture
        .grant_default_workspace_admin(&meta.workspace_id)
        .await
        .context("grant admin before adding and deciding review")?;
    add_bash_admin_review_rule(test.client(), &meta.workspace_id).await?;

    let events = run_scripted_turn(&test, session_id, "Run the clear-review bash command.").await?;
    let original_tool_id = original_tool_call_id(&events, "clear-review-bash")
        .context("expected original provider-linked tool call")?;
    let review_id = action_review_id(&events).context("expected ActionReviewRequested event")?;

    decide_review(
        test.client(),
        &meta.workspace_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await?;
    let decided_events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        decided_events.iter().any(
            |record| matches!(&record.event, Event::ActionReviewDecided { review_id: id, decision: ActionReviewDecision::Cleared, .. } if *id == review_id)
        ),
        "clear decision should be persisted: {}",
        event_summary(&decided_events)
    );
    let fresh_result = decided_events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                tool_id,
                provider_tool_use_id,
                output,
                success,
                ..
            } if provider_tool_use_id.is_none()
                && *success
                && *tool_id != original_tool_id
                && output.to_text().contains("clear-review-ok") =>
            {
                Some(*tool_id)
            }
            _ => None,
        });
    let fresh_tool_id =
        fresh_result.context("expected cleared action to execute with fresh tool id")?;
    assert!(
        decided_events.iter().any(
            |record| matches!(&record.event, Event::ToolCall { tool_id, provider_tool_use_id, tool_name, .. }
                if *tool_id == fresh_tool_id && provider_tool_use_id.is_none() && tool_name == "bash")
        ),
        "cleared action should append a fresh ToolCall before ToolResult: {}",
        event_summary(&decided_events)
    );
    Ok(())
}

async fn workspace_admin_deny_does_not_execute_stored_review_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: denying a workspace action review records the decision without executing the stored action.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-deny").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_workspace(test.client(), &meta.workspace_id).await?;
    fixture
        .grant_default_workspace_admin(&meta.workspace_id)
        .await
        .context("grant admin before adding and denying review")?;
    add_bash_admin_review_rule(test.client(), &meta.workspace_id).await?;

    let events = run_scripted_turn(&test, session_id, "Run the deny-review bash command.").await?;
    let review_id = action_review_id(&events).context("expected ActionReviewRequested event")?;
    decide_review(
        test.client(),
        &meta.workspace_id,
        review_id,
        ActionReviewDecisionKind::Denied,
        Some("risk too high"),
    )
    .await?;

    let decided_events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        decided_events.iter().any(
            |record| matches!(&record.event, Event::ActionReviewDecided { review_id: id, decision: ActionReviewDecision::Denied { .. }, .. } if *id == review_id)
        ),
        "deny decision should be persisted: {}",
        event_summary(&decided_events)
    );
    assert!(
        decided_events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { provider_tool_use_id: None, output, success: true, .. }
                if output.to_text().contains("deny-review-should-not-run")
        )),
        "denied review must not execute the stored action: {}",
        event_summary(&decided_events)
    );
    Ok(())
}

async fn workspace_member_cannot_decide_action_review(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: non-admin workspace members cannot list or decide action reviews.
    let test = fixture.isolated().await;
    let session_id = test.create_session("member-denied").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_workspace(test.client(), &meta.workspace_id).await?;
    fixture
        .grant_default_workspace_admin(&meta.workspace_id)
        .await
        .context("grant admin before adding review rule")?;
    add_bash_admin_review_rule(test.client(), &meta.workspace_id).await?;

    let events =
        run_scripted_turn(&test, session_id, "Run the member-denied bash command.").await?;
    let review_id = action_review_id(&events).context("expected ActionReviewRequested event")?;

    let member_identity = test_identity();
    fixture
        .grant_workspace_member_identity(&member_identity, &meta.workspace_id)
        .await
        .context("grant non-admin member before forbidden review decision")?;
    let member_client = TestApiClient::new(&fixture.ingress_url)?.with_identity(member_identity);

    let list_error = list_pending_reviews(&member_client, &meta.workspace_id)
        .await
        .expect_err("workspace member should not list pending action reviews");
    assert_authz_error(&list_error);
    let decide_error = decide_review(
        &member_client,
        &meta.workspace_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await
    .expect_err("workspace member should not decide action reviews");
    assert_authz_error(&decide_error);

    let after_attempt = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        after_attempt
            .iter()
            .all(|record| !matches!(record.event, Event::ActionReviewDecided { .. })),
        "failed member decision must not append a decision event: {}",
        event_summary(&after_attempt)
    );
    assert!(
        after_attempt.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { provider_tool_use_id: None, output, success: true, .. }
                if output.to_text().contains("member-denied-should-not-run")
        )),
        "failed member decision must not execute the stored action: {}",
        event_summary(&after_attempt)
    );
    Ok(())
}

async fn run_scripted_turn(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    message: &str,
) -> Result<Vec<EventRecord>> {
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                user_message: message.to_string(),
                attachments: Vec::new(),
                model: None,
            },
            None,
        )
        .await?;
    let turn_id = started
        .turn_id
        .context("start_turn should start immediately in serialized E2E")?;
    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(90),
            Duration::from_millis(250),
        )
        .await?;
    assert_eq!(
        outcome.kind,
        TurnOutcomeKind::Completed,
        "scripted turn should complete: {outcome:?}"
    );
    test.client()
        .get_events(session_id, EventRange::all())
        .await
}

async fn initialize_workspace(client: &TestApiClient, workspace_id: &WorkspaceId) -> Result<()> {
    client
        .post_void(
            &format!("/Workspace/{workspace_id}/init"),
            &WorkspaceConfig {
                id: workspace_id.clone(),
                name: format!("Action policy E2E {workspace_id}"),
                consolidation_hour_utc: 2,
                action_policy: WorkspaceActionPolicy::default(),
            },
        )
        .await
}

async fn add_bash_admin_review_rule(
    client: &TestApiClient,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    client
        .post_void(
            &format!("/Workspace/{workspace_id}/add_action_policy_rule"),
            &WorkspaceActionPolicyRuleInput {
                tool_name: "bash".to_string(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::AdminReview,
                reason: Some("E2E admin-review rule for bash".to_string()),
            },
        )
        .await
}

async fn list_pending_reviews(
    client: &TestApiClient,
    workspace_id: &WorkspaceId,
) -> Result<Vec<ActionReviewSummary>> {
    client
        .post_call(
            "/ActionReviews/list_pending",
            &ListActionReviewsRequest {
                workspace_id: workspace_id.clone(),
            },
        )
        .await
}

async fn decide_review(
    client: &TestApiClient,
    workspace_id: &WorkspaceId,
    review_id: Uuid,
    decision: ActionReviewDecisionKind,
    reason: Option<&str>,
) -> Result<()> {
    client
        .post_void(
            "/ActionReviews/decide",
            &DecideActionReviewRequest {
                workspace_id: workspace_id.clone(),
                review_id,
                decision,
                reason: reason.map(str::to_string),
            },
        )
        .await
}

fn action_policy_script() -> serde_json::Value {
    json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "responses": [
            bash_tool_response("auto-mode-bash", "printf auto-mode-ok"),
            text_response("Auto mode finished."),
            bash_tool_response("pending-review-bash", "printf should-not-run-before-clear"),
            text_response("Admin review path continued."),
            bash_tool_response("clear-review-bash", "printf clear-review-ok"),
            text_response("Clear review path continued."),
            bash_tool_response("deny-review-bash", "printf deny-review-should-not-run"),
            text_response("Deny review path continued."),
            bash_tool_response("member-denied-bash", "printf member-denied-should-not-run"),
            text_response("Member denial path continued.")
        ]
    })
}

fn bash_tool_response(provider_tool_id: &str, cmd: &str) -> serde_json::Value {
    json!({
        "completion": {
            "content": "",
            "tool_calls": [{
                "name": "bash",
                "id": provider_tool_id,
                "input": { "cmd": cmd }
            }]
        }
    })
}

fn text_response(text: &str) -> serde_json::Value {
    json!({
        "completion": {
            "content": text,
            "tool_calls": []
        }
    })
}

fn test_identity() -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn action_review_id(events: &[EventRecord]) -> Option<Uuid> {
    events.iter().find_map(|record| match &record.event {
        Event::ActionReviewRequested { review_id, .. } => Some(*review_id),
        _ => None,
    })
}

fn original_tool_call_id(events: &[EventRecord], provider_tool_use_id: &str) -> Option<ToolCallId> {
    events.iter().find_map(|record| match &record.event {
        Event::ToolCall {
            tool_id,
            provider_tool_use_id: Some(actual),
            ..
        } if actual == provider_tool_use_id => Some(*tool_id),
        _ => None,
    })
}

fn assert_tool_result(
    events: &[EventRecord],
    provider_tool_use_id: Option<&str>,
    expected_text: &str,
    expected_success: bool,
    context: &str,
) {
    assert!(
        events.iter().any(|record| match &record.event {
            Event::ToolResult {
                provider_tool_use_id: actual,
                output,
                success,
                ..
            } =>
                actual.as_deref() == provider_tool_use_id
                    && *success == expected_success
                    && output.to_text().contains(expected_text),
            _ => false,
        }),
        "expected {context}; events: {}",
        event_summary(events)
    );
}

fn assert_authz_error(error: &anyhow::Error) {
    let message = error.to_string();
    assert!(
        message.contains("403")
            || message.contains("Forbidden")
            || message.contains("forbidden")
            || message.contains("authorization")
            || message.contains("authorized"),
        "expected authorization failure, got: {message}"
    );
}

fn event_summary(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| match &record.event {
            Event::ToolCall {
                tool_id,
                provider_tool_use_id,
                tool_name,
                ..
            } => format!(
                "#{} ToolCall {tool_name} {tool_id} provider={provider_tool_use_id:?}",
                record.sequence_num
            ),
            Event::ToolResult {
                tool_id,
                provider_tool_use_id,
                success,
                output,
                ..
            } => format!(
                "#{} ToolResult {tool_id} provider={provider_tool_use_id:?} success={success} text={}",
                record.sequence_num,
                truncate_for_summary(&output.to_text())
            ),
            Event::ActionReviewRequested { review_id, .. } => {
                format!("#{} ActionReviewRequested {review_id}", record.sequence_num)
            }
            Event::ActionReviewDecided {
                review_id, decision, ..
            } => format!(
                "#{} ActionReviewDecided {review_id} {decision:?}",
                record.sequence_num
            ),
            Event::BrainResponse { text, .. } => {
                format!(
                    "#{} BrainResponse {}",
                    record.sequence_num,
                    truncate_for_summary(text)
                )
            }
            _ => format!("#{} {:?}", record.sequence_num, record.event_type),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn truncate_for_summary(value: &str) -> String {
    const LIMIT: usize = 300;
    if value.chars().count() <= LIMIT {
        return value.replace('\n', "\\n");
    }

    let truncated = value.chars().take(LIMIT).collect::<String>();
    format!("{}...", truncated.replace('\n', "\\n"))
}
