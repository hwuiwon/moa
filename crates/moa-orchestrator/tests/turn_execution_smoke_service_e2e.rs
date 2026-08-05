//! End-to-end smoke coverage for `TurnExecution` cancellation plumbing.

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::{
    types::contact::SessionActorRef, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_test_support::fixtures::fresh_client_message_id;
use moa_wire::turn::{CancelResponse, SessionSnapshot, StartTurnRequest, TurnOutcomeKind};
use uuid::Uuid;

use crate::support::restate_runtime::{grant_tenant_operator, test_user_identity, with_identity};

#[path = "support/mod.rs"]
mod support;

struct InitializedSession {
    id: String,
    identity: Identity,
}

fn ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".to_string())
}

fn workflow_url(turn_id: &str, handler: &str) -> String {
    format!(
        "{}/restate/call/TurnExecution/{turn_id}/{handler}",
        ingress_url()
    )
}

fn session_url(session_id: &str, handler: &str) -> String {
    format!(
        "{}/restate/call/Session/{session_id}/{handler}",
        ingress_url()
    )
}

fn session_store_url(handler: &str) -> String {
    format!("{}/restate/call/SessionStore/{handler}", ingress_url())
}

fn live_model() -> &'static str {
    if std::env::var("MOA_ANTHROPIC_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "claude-sonnet-4-6";
    }
    if std::env::var("MOA_OPENAI_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "gpt-5.4-mini";
    }
    if std::env::var("MOA_GOOGLE_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "gemini-3-flash-preview";
    }
    "gpt-5.4-mini"
}

async fn create_initialized_session(
    client: &reqwest::Client,
    _label: &str,
) -> Result<InitializedSession> {
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let meta = SessionMeta {
        tenant_id,
        model: ModelId::new(live_model()),
        created_by: Some(SessionActorRef::Identity { id: identity.id }),
        ..SessionMeta::default()
    };
    grant_tenant_operator(&identity, tenant_id).await?;

    let create_request = client.post(session_store_url("create_session"));
    let session_id = with_identity(create_request, &identity)
        .json(&meta)
        .send()
        .await
        .context("send SessionStore create_session")?
        .error_for_status()
        .context("SessionStore create_session should succeed")?
        .json::<SessionId>()
        .await
        .context("deserialize created session id")?;

    Ok(InitializedSession {
        id: session_id.to_string(),
        identity,
    })
}

async fn start_turn(client: &reqwest::Client, session: &InitializedSession) -> Result<String> {
    let request = client.post(session_url(&session.id, "start_turn"));
    with_identity(request, &session.identity)
        .json(&StartTurnRequest {
            client_message_id: fresh_client_message_id(),
            reply_to: None,
            stream_cursor: None,
            user_message: "smoke test".to_string(),
            attachments: Vec::new(),
            model: None,
            contact: None,
            max_turns: None,
            resource_budget: Default::default(),
            execution_template: None,
        })
        .send()
        .await
        .context("send Session start_turn")?
        .error_for_status()
        .context("Session start_turn should succeed")?
        .json::<moa_wire::turn::StartTurnResponse>()
        .await
        .context("decode Session start_turn response")?
        .turn_id
        .context("idle session should start a turn immediately")
}

async fn request_cancel(
    client: &reqwest::Client,
    session: &InitializedSession,
    reason: &str,
) -> Result<CancelResponse> {
    let request = client.post(session_url(&session.id, "request_cancel"));
    with_identity(request, &session.identity)
        .json(&serde_json::json!(reason))
        .send()
        .await
        .context("send Session request_cancel")?
        .error_for_status()
        .context("Session request_cancel should succeed")?
        .json()
        .await
        .context("decode Session request_cancel response")
}

async fn snapshot(
    client: &reqwest::Client,
    session: &InitializedSession,
) -> Result<SessionSnapshot> {
    let request = client.post(session_url(&session.id, "snapshot"));
    with_identity(request, &session.identity)
        .send()
        .await
        .context("send Session snapshot")?
        .error_for_status()
        .context("Session snapshot should succeed")?
        .json::<SessionSnapshot>()
        .await
        .context("deserialize Session snapshot")
}

async fn await_cancelled(
    client: &reqwest::Client,
    session: &InitializedSession,
    turn_id: &str,
    timeout: Duration,
) -> Result<SessionSnapshot> {
    let deadline = Instant::now() + timeout;
    let mut last_snapshot = None;
    while Instant::now() < deadline {
        let current = snapshot(client, session).await?;
        if current.last_outcome.as_ref().is_some_and(|outcome| {
            outcome.turn_id == turn_id && outcome.kind == TurnOutcomeKind::Cancelled
        }) {
            return Ok(current);
        }
        last_snapshot = Some(current);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    bail!(
        "turn {turn_id} did not finish cancellation within {:?}; last snapshot: {:?}",
        timeout,
        last_snapshot
    )
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn session_cancel_after_start_turn_cancels_private_workflow() -> Result<()> {
    // Pins: the public Session owns cancellation and reaches its private
    // TurnExecution child without exposing workflow ingress.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "turn-after").await?;

    let turn_id = start_turn(&client, &session).await?;
    let cancel = request_cancel(&client, &session, "after-init").await?;
    assert!(cancel.cancelled);
    assert_eq!(cancel.reason, format!("cancel forwarded to turn {turn_id}"));
    let cancelled = await_cancelled(&client, &session, &turn_id, Duration::from_secs(10)).await?;
    assert_eq!(cancelled.active_turn_id, None);
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn session_cancel_without_active_turn_is_a_noop() -> Result<()> {
    // Pins: public cancellation never fabricates a private workflow invocation.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "turn-before").await?;

    let cancel = request_cancel(&client, &session, "before-init").await?;
    assert!(!cancel.cancelled);
    assert_eq!(cancel.reason, "no active turn");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and registered moa-orchestrator deployment"]
async fn direct_turn_execution_ingress_is_rejected() -> Result<()> {
    // Pins: the workflow remains callable only from Session service-to-service
    // dispatch; public ingress cannot start it with forged owner coordinates.
    let turn_id = format!("private-probe-{}", Uuid::now_v7());
    let response = reqwest::Client::new()
        .post(workflow_url(&turn_id, "run"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .context("probe private TurnExecution ingress")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read private TurnExecution rejection")?;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "unexpected private TurnExecution rejection body: {body}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body)
            .context("decode private TurnExecution rejection")?,
        serde_json::json!({"message": "the invoked service is not public"}),
        "Restate must reject the request at its private-ingress boundary"
    );
    Ok(())
}
