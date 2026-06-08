//! HTTP-backed runtime smoke tests.

use moa_core::{
    MoaConfig, ModelId, Platform, RuntimeEvent, SessionId, SessionMeta, UserId, WorkspaceId,
};
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
async fn attach_to_session_loads_existing_session_without_creating_one() {
    // Pins: explicit attach loads existing session metadata through the orchestrator API.
    let mut server = Server::new_async().await;
    let session_id = SessionId::new();
    let meta = SessionMeta {
        id: session_id,
        workspace_id: WorkspaceId::new("attached-workspace"),
        user_id: UserId::new("attached-user"),
        model: ModelId::new("attached-model"),
        ..SessionMeta::default()
    };
    let get_session_mock = server
        .mock("POST", "/SessionStore/get_session")
        .match_body(Matcher::Exact(format!("\"{session_id}\"")))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&meta).expect("serialize session meta"))
        .create_async()
        .await;
    let tools_mock = server
        .mock("POST", "/ToolExecutor/list_tools")
        .match_body(Matcher::Exact(r#""attached-workspace""#.to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(tool_descriptor_body())
        .create_async()
        .await;
    let mut config = MoaConfig::default();
    config.orchestrator.endpoint = Some(server.url());

    let runtime = ChatRuntime::attach_to_session(config, Platform::Cli, session_id)
        .await
        .expect("runtime should attach through mocked orchestrator");

    assert_eq!(runtime.session_id(), &session_id);
    assert_eq!(runtime.workspace_id().as_str(), "attached-workspace");
    assert_eq!(runtime.model(), "attached-model");
    assert_eq!(runtime.tool_names(), vec!["bash".to_string()]);
    get_session_mock.assert_async().await;
    tools_mock.assert_async().await;
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
