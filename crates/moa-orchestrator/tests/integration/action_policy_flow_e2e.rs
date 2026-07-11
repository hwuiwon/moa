//! End-to-end action-policy and tenant-admin review coverage.

use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::turn::{StartTurnRequest, TurnOutcomeKind};
use moa_core::{
    events::Event, types::action_policy::ActionPolicyEffect,
    types::action_policy::ActionReviewDecision, types::action_policy::ActionReviewStatus,
    types::completion::ToolInvocation, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::SessionId, types::identifiers::TenantId,
    types::identifiers::ToolCallId, types::identifiers::UserId, types::session::SessionStatus,
    types::tools::ToolCallRequest,
};
use moa_orchestrator::objects::tenant::TenantConfig;
use moa_orchestrator::services::action_policy::{PrepareActionReviewRequest, PreparedActionReview};
use moa_orchestrator::services::action_reviews::{
    ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest,
    ListActionReviewsRequest, RequestActionReview,
};
use moa_test_support::{IsolatedTest, OrchestratorTestFixture, TestApiClient};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn action_policy_flow_covers_auto_review_decision_and_member_authz() -> Result<()> {
    // Pins: action-policy E2E scenarios share one scripted fixture process.
    let fixture = OrchestratorTestFixture::with_script(action_policy_script()).await?;
    action_policy_auto_mode_executes_shell_without_user_approval(&fixture).await?;
    admin_review_policy_records_pending_review_and_turn_continues(&fixture).await?;
    tenant_admin_clear_executes_stored_review_action(&fixture).await?;
    tenant_admin_deny_does_not_execute_stored_review_action(&fixture).await?;
    tenant_member_cannot_decide_action_review(&fixture).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn scripted_provider_repeated_tool_loop_stops_before_unbounded_dispatch() -> Result<()> {
    // Pins: scripted providers that keep requesting the same tool stop in TurnExecution.
    let fixture = OrchestratorTestFixture::with_script(repeated_tool_loop_script()).await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("repeated-tool-loop").await?;

    let events = run_scripted_turn(
        &test,
        session_id,
        "run cargo test -p moa-orchestrator for repeated-loop fixture",
    )
    .await?;

    let repeated_tool_ids = repeated_loop_tool_call_provider_ids(&events);
    assert_eq!(
        repeated_tool_ids,
        vec!["repeated-loop-1", "repeated-loop-2"],
        "third identical scripted tool request should stop before ToolCall persistence: {}",
        event_summary(&events)
    );
    assert!(
        repeated_tool_ids.len() < REPEATED_LOOP_SCRIPTED_ATTEMPTS,
        "actual ToolCall count must stay below scripted attempts: {}",
        event_summary(&events)
    );
    // The sandbox-free root coordinator denies hand-routed bash outright, so no
    // repeated call ever executes; loop detection must still stop the dispatch
    // stream on identical denied calls.
    assert_eq!(
        repeated_loop_successful_results(&events),
        0,
        "coordinator bash calls should be denied, not executed: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { output, .. }
                if output.to_text().contains("repeated-loop-ok")
        )),
        "denied bash must never reach a sandbox: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::Error { message, recoverable }
                if *recoverable
                    && message == "tool loop detected: `bash` repeated 3 consecutive times with threshold 3"
        )),
        "loop stop should append a recoverable budget error: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::BrainResponse { text, .. }
                if text == "MOA stopped before running another tool because the model repeatedly requested the same `bash` call. Narrow the scope or ask MOA to continue."
        )),
        "loop stop should append the assistant stop response: {}",
        event_summary(&events)
    );
    Ok(())
}

async fn action_policy_auto_mode_executes_shell_without_user_approval(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: high-risk command execution defaults to review, so only an
    // explicit tenant allow rule lets a delegated worker's known bash action
    // execute without approval.
    let test = fixture.isolated().await;
    let session_id = test.create_session("auto-mode").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding explicit allow rule")?;
    add_bash_allow_rule(test.client(), meta.tenant_id, "printf auto-mode-ok").await?;

    run_scripted_turn(&test, session_id, "Run the auto-mode bash command.").await?;

    let events = wait_for_events(&test, session_id, |events| {
        has_tool_result(events, Some("auto-mode-bash"), "auto-mode-ok", true)
    })
    .await
    .context("worker bash result should reach the session log")?;
    assert!(
        events
            .iter()
            .all(|record| !matches!(record.event, Event::ActionReviewRequested { .. })),
        "auto mode should not create an action review: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::ToolCall { tool_name, .. } if tool_name == "spawn_worker"
        )),
        "coordinator should delegate compute to a worker: {}",
        event_summary(&events)
    );
    Ok(())
}

async fn admin_review_policy_records_pending_review_and_turn_continues(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: a delegated worker's bash hitting an admin-review rule records a
    // pending tenant action review without blocking the worker or the session.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-pending").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding and listing pending review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    run_scripted_turn(&test, session_id, "Run the admin-review bash command.").await?;

    let events = wait_for_events(&test, session_id, |events| {
        action_review_id(events).is_some()
    })
    .await
    .context("worker bash should request an admin review")?;
    let review_id = action_review_id(&events).context("expected ActionReviewRequested event")?;
    let pending = list_pending_reviews(test.client(), meta.tenant_id).await?;
    assert_eq!(
        pending.len(),
        1,
        "expected exactly one pending review; pending: {pending:#?}; events: {}",
        event_summary(&events)
    );
    assert_eq!(pending[0].id, review_id);
    assert_eq!(pending[0].status, ActionReviewStatus::Pending);

    assert_tool_result(
        &events,
        Some("pending-review-bash"),
        "pending tenant admin review",
        false,
        "pending-review tool result",
    );
    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { output, success: true, .. }
                if output.to_text().contains("should-not-run-before-clear")
        )),
        "gated bash must not execute before the review is cleared: {}",
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

async fn tenant_admin_clear_executes_stored_review_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: clearing a tenant action review executes the stored request with a fresh tool id.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-clear").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding and deciding review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (review_id, stored_tool_id) =
        create_pending_bash_review(&test, session_id, "printf clear-review-ok").await?;
    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert_eq!(
        action_review_id(&events),
        Some(review_id),
        "review request should append ActionReviewRequested: {}",
        event_summary(&events)
    );
    // Sessions in this fixture share the default tenant, so earlier scenarios
    // may have left their own pending reviews; assert on ours by id.
    let pending = list_pending_reviews(test.client(), meta.tenant_id).await?;
    let stored = pending
        .iter()
        .find(|review| review.id == review_id)
        .context("stored review should be listed as pending before the decision")?;
    assert_eq!(stored.status, ActionReviewStatus::Pending);

    decide_review(
        test.client(),
        meta.tenant_id,
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
    // The decision path re-mints the stored request's tool id so the execution
    // can never collide with the original tool-call events.
    let fresh_tool_id = decided_events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                tool_id,
                provider_tool_use_id,
                output,
                success: true,
                ..
            } if provider_tool_use_id.is_none()
                && *tool_id != stored_tool_id
                && output.to_text().contains("clear-review-ok") =>
            {
                Some(*tool_id)
            }
            _ => None,
        })
        .context("expected cleared action to execute with a fresh tool id")?;
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

async fn tenant_admin_deny_does_not_execute_stored_review_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: denying a tenant action review records the decision without executing the stored action.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-deny").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding and denying review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (review_id, _stored_tool_id) =
        create_pending_bash_review(&test, session_id, "printf deny-review-should-not-run").await?;
    decide_review(
        test.client(),
        meta.tenant_id,
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

async fn tenant_member_cannot_decide_action_review(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: non-admin tenant operators cannot list or decide action reviews.
    let test = fixture.isolated().await;
    let session_id = test.create_session("member-denied").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding review rule")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (review_id, _stored_tool_id) =
        create_pending_bash_review(&test, session_id, "printf member-denied-should-not-run")
            .await?;

    let mut member_identity = test_identity();
    member_identity.tenant_id = meta.tenant_id;
    fixture
        .grant_tenant_operator_identity(&member_identity, meta.tenant_id)
        .await
        .context("grant non-admin member before forbidden review decision")?;
    let member_client = TestApiClient::new(&fixture.ingress_url)?.with_identity(member_identity);

    let list_error = list_pending_reviews(&member_client, meta.tenant_id)
        .await
        .expect_err("tenant operator should not list pending action reviews");
    assert_authz_error(&list_error);
    let decide_error = decide_review(
        &member_client,
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await
    .expect_err("tenant operator should not decide action reviews");
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
                contact: None,
                max_turns: None,
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

/// Mirrors the orchestrator's fallback tool `user_id` derivation for a session.
fn fallback_tool_user_id(meta: &moa_core::types::session::SessionMeta) -> UserId {
    match &meta.created_by {
        Some(moa_core::types::contact::SessionActorRef::Identity { id }) => {
            UserId::new(id.to_string())
        }
        Some(moa_core::types::contact::SessionActorRef::Contact { id }) => {
            UserId::new(format!("contact:{id}"))
        }
        Some(moa_core::types::contact::SessionActorRef::Anonymous) | None => {
            UserId::new(format!("tenant:{}", meta.tenant_id))
        }
    }
}

/// Creates a pending admin review for a worker-scoped bash invocation by
/// driving the same `ActionPolicy/prepare_action_review` and
/// `ActionReviews/request` calls the turn workflows make, returning the review
/// id and the stored tool-call id that executes if the review is cleared.
async fn create_pending_bash_review(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    cmd: &str,
) -> Result<(Uuid, ToolCallId)> {
    let meta = test.client().get_session(session_id).await?;
    let review_id = Uuid::new_v4();
    let tool_call_id = ToolCallId::new();
    let prepared: PreparedActionReview = test
        .client()
        .post_call(
            "/ActionPolicy/prepare_action_review",
            &PrepareActionReviewRequest {
                session: meta.clone(),
                invocation: ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": cmd }),
                },
                review_id,
                tool_call_id,
                worker_id: None,
                origin_kind: None,
                origin_id: None,
                origin_step_id: None,
                idempotency_key: None,
            },
        )
        .await
        .context("prepare action review via ActionPolicy")?;
    assert_eq!(
        prepared.effect,
        ActionPolicyEffect::AdminReview,
        "bash admin-review rule should match the invocation"
    );

    let tool_request = ToolCallRequest {
        tool_call_id,
        provider_tool_use_id: None,
        tool_name: "bash".to_string(),
        input: json!({ "cmd": cmd }),
        active_canary: None,
        session_id: Some(session_id),
        tenant_id: meta.tenant_id,
        user_id: fallback_tool_user_id(&meta),
        trusted_sandbox_manifest: None,
        worker_id: Some("worker-action-policy-e2e".to_string()),
    };
    let summary: ActionReviewSummary = test
        .client()
        .post_call(
            "/ActionReviews/request",
            &RequestActionReview {
                envelope: prepared.envelope,
                preview: prepared.preview,
                tool_request,
            },
        )
        .await
        .context("store pending review via ActionReviews")?;
    assert_eq!(
        summary.id, review_id,
        "stored review should keep the prepared id"
    );
    Ok((review_id, tool_call_id))
}

async fn initialize_tenant(client: &TestApiClient, tenant_id: TenantId) -> Result<()> {
    client
        .post_void(
            &format!("/Tenant/{tenant_id}/init"),
            &TenantConfig {
                id: tenant_id,
                name: format!("Action policy E2E {tenant_id}"),
                consolidation_hour_utc: 2,
            },
        )
        .await
}

async fn add_bash_admin_review_rule(client: &TestApiClient, tenant_id: TenantId) -> Result<()> {
    client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &TestRuleRequest {
                tenant_id,
                tool_name: "bash".to_string(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::AdminReview,
                reason: Some("E2E admin-review rule for bash".to_string()),
            },
        )
        .await
}

async fn add_bash_allow_rule(
    client: &TestApiClient,
    tenant_id: TenantId,
    pattern: &str,
) -> Result<()> {
    client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &TestRuleRequest {
                tenant_id,
                tool_name: "bash".to_string(),
                pattern: pattern.to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("E2E explicit allow rule for known bash command".to_string()),
            },
        )
        .await
}

#[derive(Serialize)]
struct TestRuleRequest {
    tenant_id: TenantId,
    tool_name: String,
    pattern: String,
    effect: ActionPolicyEffect,
    reason: Option<String>,
}

async fn list_pending_reviews(
    client: &TestApiClient,
    tenant_id: TenantId,
) -> Result<Vec<ActionReviewSummary>> {
    client
        .post_call(
            "/ActionReviews/list_pending",
            &ListActionReviewsRequest { tenant_id },
        )
        .await
}

async fn decide_review(
    client: &TestApiClient,
    tenant_id: TenantId,
    review_id: Uuid,
    decision: ActionReviewDecisionKind,
    reason: Option<&str>,
) -> Result<()> {
    client
        .post_void(
            "/ActionReviews/decide",
            &DecideActionReviewRequest {
                tenant_id,
                review_id,
                decision,
                reason: reason.map(str::to_string),
            },
        )
        .await
}

/// Task strings delegated to scripted workers. These are the keyed-match
/// needles for worker turns, so they must not be substrings of the user
/// messages that key the coordinator's spawn responses (or vice versa).
const AUTO_WORKER_TASK: &str = "execute the auto-mode bash probe";
const REVIEW_WORKER_TASK: &str = "execute the review-mode bash probe";
/// Worker final texts; the coordinator's post-completion resume keys on them.
const AUTO_WORKER_DONE: &str = "Auto-mode worker finished its delegated task.";
const REVIEW_WORKER_DONE: &str = "Admin-review worker finished its delegated task.";

/// Fully keyed script for the delegation-based action-policy scenarios.
///
/// Every response is keyed (no FIFO) so internal model calls the loop makes
/// (query rewrite, segment assessment, …) can never consume a scenario
/// response out of order — they fall through to `default`. First match wins,
/// so later-state needles (tool outputs, worker final texts) are registered
/// before the broader needles (task text, user message) they co-occur with.
fn action_policy_script() -> serde_json::Value {
    json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "keyed": [
            // Coordinator iterations after a spawn (and post-completion
            // resumes, whose compiled context replays the spawn result): the
            // spawn tool output summary is the only pre-worker marker.
            {
                "match": "Spawned worker",
                "completion": { "content": "Delegated; waiting for the worker." }
            },
            // Worker follow-up iterations key on their bash tool result…
            {
                "match": "auto-mode-ok",
                "completion": { "content": AUTO_WORKER_DONE }
            },
            {
                "match": "pending tenant admin review",
                "completion": { "content": REVIEW_WORKER_DONE }
            },
            // …worker first iterations on the delegated task text…
            {
                "match": AUTO_WORKER_TASK,
                "completion": {
                    "content": "Running the auto-mode probe.",
                    "tool_calls": [{
                        "name": "bash",
                        "input": { "cmd": "printf auto-mode-ok" },
                        "id": "auto-mode-bash"
                    }]
                }
            },
            {
                "match": REVIEW_WORKER_TASK,
                "completion": {
                    "content": "Running the review-mode probe.",
                    "tool_calls": [{
                        "name": "bash",
                        "input": { "cmd": "printf should-not-run-before-clear" },
                        "id": "pending-review-bash"
                    }]
                }
            },
            // …and coordinator first iterations on the user message.
            {
                "match": "Run the auto-mode bash command.",
                "completion": {
                    "content": "Delegating to the auto-mode worker.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "input": {
                            "task": AUTO_WORKER_TASK,
                            "tool_subset": ["bash"],
                            "max_turns": 3
                        },
                        "id": "spawn-auto"
                    }]
                }
            },
            {
                "match": "Run the admin-review bash command.",
                "completion": {
                    "content": "Delegating to the admin-review worker.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "input": {
                            "task": REVIEW_WORKER_TASK,
                            "tool_subset": ["bash"],
                            "max_turns": 3
                        },
                        "id": "spawn-review"
                    }]
                }
            }
        ]
    })
}

const REPEATED_LOOP_SCRIPTED_ATTEMPTS: usize = 5;
const REPEATED_LOOP_CMD: &str = "printf repeated-loop-ok";

fn repeated_tool_loop_script() -> serde_json::Value {
    let responses = (1..=REPEATED_LOOP_SCRIPTED_ATTEMPTS)
        .map(|index| bash_tool_response(&format!("repeated-loop-{index}"), REPEATED_LOOP_CMD))
        .collect::<Vec<_>>();
    json!({
        "default": {
            "completion": {
                "content": "unexpected fallback",
                "tool_calls": []
            }
        },
        "responses": responses
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

fn repeated_loop_tool_call_provider_ids(events: &[EventRecord]) -> Vec<&str> {
    let expected_input = json!({ "cmd": REPEATED_LOOP_CMD });
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                provider_tool_use_id,
                tool_name,
                input,
                ..
            } if tool_name == "bash" && input == &expected_input => provider_tool_use_id.as_deref(),
            _ => None,
        })
        .collect()
}

fn repeated_loop_successful_results(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ToolResult {
                    provider_tool_use_id: Some(provider_tool_use_id),
                    output,
                    success: true,
                    ..
                } if provider_tool_use_id.starts_with("repeated-loop-")
                    && output.to_text().contains("repeated-loop-ok")
            )
        })
        .count()
}

fn test_identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id: TenantId::new(),
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

/// Returns whether a matching `ToolResult` event exists in the session log.
fn has_tool_result(
    events: &[EventRecord],
    provider_tool_use_id: Option<&str>,
    expected_text: &str,
    expected_success: bool,
) -> bool {
    events.iter().any(|record| match &record.event {
        Event::ToolResult {
            provider_tool_use_id: actual,
            output,
            success,
            ..
        } => {
            actual.as_deref() == provider_tool_use_id
                && *success == expected_success
                && output.to_text().contains(expected_text)
        }
        _ => false,
    })
}

/// Polls the session event log until `predicate` holds, returning the events.
///
/// Worker turns run in their own Restate invocations, so their events land in
/// the parent session log after the coordinator turn has already completed.
async fn wait_for_events<F>(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    predicate: F,
) -> Result<Vec<EventRecord>>
where
    F: Fn(&[EventRecord]) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let events = test
            .client()
            .get_events(session_id, EventRange::all())
            .await?;
        if predicate(&events) {
            return Ok(events);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for session events: {}",
                event_summary(&events)
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn assert_tool_result(
    events: &[EventRecord],
    provider_tool_use_id: Option<&str>,
    expected_text: &str,
    expected_success: bool,
    context: &str,
) {
    assert!(
        has_tool_result(
            events,
            provider_tool_use_id,
            expected_text,
            expected_success
        ),
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
            Event::Error {
                message,
                recoverable,
            } => format!(
                "#{} Error recoverable={recoverable} {}",
                record.sequence_num,
                truncate_for_summary(message)
            ),
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
