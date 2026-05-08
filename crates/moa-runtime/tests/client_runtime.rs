//! HTTP-backed runtime smoke tests.

use moa_core::{MoaConfig, Platform, RuntimeEvent};
use moa_runtime::ChatRuntime;
use mockito::{Matcher, Server};
use tokio::sync::mpsc;

fn tool_descriptor_body() -> &'static str {
    r#"[{"name":"bash","description":"run shell commands","schema":{},"idempotency_class":"idempotent","requires_approval":false}]"#
}

async fn mock_runtime_bootstrap(server: &mut Server) {
    server
        .mock("POST", "/SessionStore/create_session")
        .match_body(Matcher::PartialJson(
            serde_json::json!({"platform":"cli","status":"created"}),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(r#""{}""#, uuid::Uuid::now_v7()))
        .create_async()
        .await;
    server
        .mock("POST", "/SessionStore/append_event")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("1")
        .create_async()
        .await;
    server
        .mock("POST", "/SessionStore/init_session_vo")
        .with_status(200)
        .create_async()
        .await;
    server
        .mock("POST", "/ToolExecutor/list_tools")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(tool_descriptor_body())
        .create_async()
        .await;
}

#[tokio::test]
async fn from_endpoint_creates_initial_session_and_caches_tool_names() {
    // Pins: ChatRuntime construction creates a remote session and caches ToolExecutor names.
    let mut server = Server::new_async().await;
    mock_runtime_bootstrap(&mut server).await;

    let runtime = ChatRuntime::from_endpoint(MoaConfig::default(), Platform::Cli, server.url())
        .await
        .expect("runtime should initialize through mocked orchestrator");

    assert_eq!(runtime.tool_names(), vec!["bash".to_string()]);
}

#[tokio::test]
async fn run_turn_queues_message_and_relays_completed_outcome_as_runtime_events() {
    // Pins: run_turn uses Session/queue_message and converts the terminal snapshot into CLI events.
    let mut server = Server::new_async().await;
    mock_runtime_bootstrap(&mut server).await;
    let runtime = ChatRuntime::from_endpoint(MoaConfig::default(), Platform::Cli, server.url())
        .await
        .expect("runtime should initialize through mocked orchestrator");
    let session_id = runtime.session_id().to_string();

    let queue_mock = server
        .mock(
            "POST",
            format!("/Session/{session_id}/queue_message").as_str(),
        )
        .match_body(Matcher::PartialJson(
            serde_json::json!({"user_message":"hello"}),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"queued":false,"started_turn_id":"turn-1"}"#)
        .create_async()
        .await;
    let snapshot_mock = server
        .mock("POST", format!("/Session/{session_id}/snapshot").as_str())
        .match_body(Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"session_id":"{session_id}","active_turn_id":null,"pending_message_count":0,"last_outcome":{{"turn_id":"turn-1","kind":"Completed","message":"ok"}}}}"#
        ))
        .create_async()
        .await;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    runtime
        .run_turn("hello".to_string(), event_tx)
        .await
        .expect("run_turn should complete from mocked outcome");

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }

    assert_eq!(
        events,
        vec![
            RuntimeEvent::AssistantStarted,
            RuntimeEvent::AssistantDelta('o'),
            RuntimeEvent::AssistantDelta('k'),
            RuntimeEvent::AssistantFinished {
                text: "ok".to_string()
            },
            RuntimeEvent::TurnCompleted,
        ]
    );
    queue_mock.assert_async().await;
    snapshot_mock.assert_async().await;
}
