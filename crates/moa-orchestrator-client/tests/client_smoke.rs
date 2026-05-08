use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use moa_orchestrator_client::*;
use mockito::{Matcher, Server};

fn env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env test lock should not be poisoned")
}

fn clear_endpoint_env() {
    // SAFETY: these tests serialize environment mutation through `env_guard`.
    unsafe {
        std::env::remove_var("MOA__ORCHESTRATOR__ENDPOINT");
        std::env::remove_var("RESTATE_INGRESS_URL");
    }
}

#[tokio::test]
async fn from_env_reads_endpoint() {
    // Pins: MOA__ORCHESTRATOR__ENDPOINT takes precedence over defaults.
    let _guard = env_guard();
    clear_endpoint_env();
    // SAFETY: this test holds `env_guard` while mutating process environment.
    unsafe {
        std::env::set_var("MOA__ORCHESTRATOR__ENDPOINT", "http://example:1234");
    }

    let client = OrchestratorClient::from_env().expect("endpoint env should build client");

    assert_eq!(client.endpoint(), "http://example:1234");
    clear_endpoint_env();
}

#[tokio::test]
async fn from_env_reads_restate_ingress_fallback() {
    // Pins: RESTATE_INGRESS_URL is honored when the MOA-specific env var is unset.
    let _guard = env_guard();
    clear_endpoint_env();
    // SAFETY: this test holds `env_guard` while mutating process environment.
    unsafe {
        std::env::set_var("RESTATE_INGRESS_URL", "http://restate.example:18080");
    }

    let client = OrchestratorClient::from_env().expect("restate env should build client");

    assert_eq!(client.endpoint(), "http://restate.example:18080");
    clear_endpoint_env();
}

#[tokio::test]
async fn from_env_defaults_to_compose_ingress_when_unset() {
    // Pins: the client is usable in compose without requiring local env setup.
    let _guard = env_guard();
    clear_endpoint_env();

    let client = OrchestratorClient::from_env().expect("default endpoint should build client");

    assert_eq!(client.endpoint(), "http://localhost:18080");
}

#[tokio::test]
async fn start_turn_posts_to_correct_path_with_idempotency_key() {
    // Pins: start_turn uses the Session request-response endpoint and threads idempotency.
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/Session/sess-1/start_turn")
        .match_header("idempotency-key", "req-1")
        .match_body(Matcher::PartialJson(
            serde_json::json!({"user_message": "hi"}),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"turn_id":"t-1","queued":false}"#)
        .create_async()
        .await;

    let client = OrchestratorClient::new(server.url()).expect("mock endpoint should parse");
    let response = client
        .session("sess-1")
        .start_turn(
            StartTurnRequest {
                user_message: "hi".to_string(),
                attachments: Vec::new(),
                model: None,
            },
            Some("req-1"),
        )
        .await
        .expect("start_turn should decode mock response");

    assert_eq!(
        response,
        StartTurnResponse {
            turn_id: Some("t-1".to_string()),
            queued: false,
        }
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn queue_message_posts_to_correct_path_with_idempotency_key() {
    // Pins: queue_message uses the Session queue endpoint and threads idempotency.
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/Session/sess-1/queue_message")
        .match_header("idempotency-key", "req-2")
        .match_body(Matcher::PartialJson(
            serde_json::json!({"user_message": "second"}),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"queued":true,"started_turn_id":null}"#)
        .create_async()
        .await;

    let client = OrchestratorClient::new(server.url()).expect("mock endpoint should parse");
    let response = client
        .session("sess-1")
        .queue_message(
            QueueMessageRequest {
                user_message: "second".to_string(),
                attachments: Vec::new(),
                model: None,
            },
            Some("req-2"),
        )
        .await
        .expect("queue_message should decode mock response");

    assert_eq!(
        response,
        QueueMessageResponse {
            queued: true,
            started_turn_id: None,
        }
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn request_cancel_posts_string_body_and_decodes_response() {
    // Pins: request_cancel forwards the string reason body expected by the Session handler.
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/Session/sess-1/request_cancel")
        .match_body(Matcher::Exact("\"stop now\"".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"cancelled":true,"reason":"cancel forwarded to turn t-1"}"#)
        .create_async()
        .await;

    let client = OrchestratorClient::new(server.url()).expect("mock endpoint should parse");
    let response = client
        .session("sess-1")
        .request_cancel("stop now")
        .await
        .expect("request_cancel should decode mock response");

    assert_eq!(
        response,
        CancelResponse {
            cancelled: true,
            reason: "cancel forwarded to turn t-1".to_string(),
        }
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn snapshot_posts_empty_body_and_decodes_state() {
    // Pins: snapshot calls the no-input shared Session handler with an empty body.
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/Session/sess-1/snapshot")
        .match_header("content-type", Matcher::Missing)
        .match_body(Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"session_id":"sess-1","active_turn_id":"t-1","pending_message_count":2,"last_outcome":null}"#,
        )
        .create_async()
        .await;

    let client = OrchestratorClient::new(server.url()).expect("mock endpoint should parse");
    let snapshot = client
        .session("sess-1")
        .snapshot()
        .await
        .expect("snapshot should decode mock response");

    assert_eq!(snapshot.session_id, "sess-1");
    assert_eq!(snapshot.active_turn_id.as_deref(), Some("t-1"));
    assert_eq!(snapshot.pending_message_count, 2);
    assert_eq!(snapshot.last_outcome, None);
    mock.assert_async().await;
}

#[tokio::test]
async fn await_turn_outcome_returns_matching_terminal_outcome() {
    // Pins: await_turn_outcome returns only the requested turn's visible outcome.
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/Session/sess-1/snapshot")
        .match_header("content-type", Matcher::Missing)
        .match_body(Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"session_id":"sess-1","active_turn_id":null,"pending_message_count":0,"last_outcome":{"turn_id":"t-1","kind":"Completed","message":"done"}}"#,
        )
        .create_async()
        .await;

    let client = OrchestratorClient::new(server.url()).expect("mock endpoint should parse");
    let outcome = client
        .session("sess-1")
        .await_turn_outcome("t-1", Duration::from_secs(1), Duration::from_millis(1))
        .await
        .expect("matching outcome should be returned");

    assert_eq!(
        outcome,
        TurnOutcome {
            turn_id: "t-1".to_string(),
            kind: TurnOutcomeKind::Completed,
            message: "done".to_string(),
        }
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn bad_status_preserves_status_and_body() {
    // Pins: non-2xx Restate responses surface status and body without lossy wrapping.
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/Session/sess-1/snapshot")
        .with_status(503)
        .with_body("deployment unavailable")
        .create_async()
        .await;

    let client = OrchestratorClient::new(server.url()).expect("mock endpoint should parse");
    let error = client
        .session("sess-1")
        .snapshot()
        .await
        .expect_err("snapshot should fail on non-2xx status");

    match error {
        Error::BadStatus { status, body } => {
            assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(body, "deployment unavailable");
        }
        other => panic!("expected BadStatus, got {other:?}"),
    }
    mock.assert_async().await;
}

#[cfg(feature = "integration")]
#[tokio::test]
#[ignore = "requires make dev, MOA_RUN_LIVE_ORCHESTRATOR_CLIENT_TESTS=1, and configured providers"]
async fn live_start_turn_returns_fast_with_initialized_session() {
    use chrono::Utc;
    use moa_core::{ModelId, Platform, SessionId, SessionMeta, SessionStatus, UserId, WorkspaceId};

    if std::env::var("MOA_RUN_LIVE_ORCHESTRATOR_CLIENT_TESTS").as_deref() != Ok("1") {
        return;
    }

    let client = OrchestratorClient::from_env().expect("live endpoint should build client");
    let session_id = SessionId::new();
    let now = Utc::now();
    let meta = SessionMeta {
        id: session_id,
        workspace_id: WorkspaceId::new("client-live"),
        user_id: UserId::new("client-live"),
        title: Some("client live smoke".to_string()),
        status: SessionStatus::Created,
        platform: Platform::Cli,
        platform_channel: None,
        model: ModelId::new("default"),
        created_at: now,
        updated_at: now,
        completed_at: None,
        parent_session_id: None,
        total_input_tokens: 0,
        total_input_tokens_uncached: 0,
        total_input_tokens_cache_write: 0,
        total_input_tokens_cache_read: 0,
        total_output_tokens: 0,
        total_cost_cents: 0,
        event_count: 0,
        last_checkpoint_seq: None,
    };

    let http = reqwest::Client::new();
    http.post(format!("{}/SessionStore/create_session", client.endpoint()))
        .json(&meta)
        .send()
        .await
        .expect("create_session request should complete")
        .error_for_status()
        .expect("create_session should succeed");
    http.post(format!(
        "{}/Session/{session_id}/set_meta",
        client.endpoint()
    ))
    .json(&meta)
    .send()
    .await
    .expect("set_meta request should complete")
    .error_for_status()
    .expect("set_meta should succeed");

    let started_at = std::time::Instant::now();
    let response = client
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                user_message: "What is 2+2? Respond with just the number.".to_string(),
                attachments: Vec::new(),
                model: None,
            },
            Some("client-live-start"),
        )
        .await
        .expect("start_turn should return a turn id");

    assert!(
        started_at.elapsed() < Duration::from_secs(1),
        "start_turn should be fast"
    );
    assert!(response.turn_id.is_some(), "expected a turn id");
    assert!(!response.queued, "first turn should start immediately");
}
