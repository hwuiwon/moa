use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, Event, EventRange, LLMProvider, MoaConfig, Platform,
    Result, SessionSignal, SessionStatus, SessionStore, StartSessionRequest, ToolOutput, UserId,
    UserMessage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::LocalOrchestrator;
use moa_providers::{AnthropicProvider, GeminiProvider, ModelRouter, OpenAIProvider};
use moa_session::{PostgresSessionStore, testing};
use tempfile::TempDir;
use tokio::time::{Instant, sleep};

struct LiveProvider {
    label: &'static str,
    model: String,
    provider: Arc<dyn LLMProvider>,
}

fn available_live_providers() -> Vec<LiveProvider> {
    let mut providers = Vec::new();
    if let Ok(provider) = OpenAIProvider::from_env("gpt-5.4") {
        providers.push(LiveProvider {
            label: "openai",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    if let Ok(provider) = AnthropicProvider::from_env("claude-sonnet-4-6") {
        providers.push(LiveProvider {
            label: "anthropic",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    if let Ok(provider) = GeminiProvider::from_env("gemini-3.1-pro-preview") {
        providers.push(LiveProvider {
            label: "google",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    providers
}

fn google_live_provider() -> Option<LiveProvider> {
    GeminiProvider::from_env("gemini-3.1-pro-preview")
        .ok()
        .map(|provider| LiveProvider {
            label: "google",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        })
}

async fn live_orchestrator_with_provider(
    provider: Arc<dyn LLMProvider>,
) -> Result<(TempDir, Arc<PostgresSessionStore>, LocalOrchestrator)> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let (session_store, _database_url, schema_name) = testing::create_isolated_test_store().await?;
    let session_store = Arc::new(session_store);
    let memory_store = Arc::new(
        FileMemoryStore::from_config_with_pool(
            &config,
            Arc::new(session_store.pool().clone()),
            Some(&schema_name),
        )
        .await?,
    );
    let tool_router = Arc::new(
        ToolRouter::from_config(&config, memory_store.clone())
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store.clone(),
        memory_store,
        Arc::new(ModelRouter::new(provider, None)),
        tool_router,
    )
    .await?;
    Ok((dir, session_store, orchestrator))
}

async fn wait_for_status(
    orchestrator: &LocalOrchestrator,
    session_store: &PostgresSessionStore,
    session_id: moa_core::SessionId,
    expected: SessionStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let meta = orchestrator
            .get_session(session_id)
            .await
            .expect("session metadata");
        if meta.status == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for session {} status {:?}; current status {:?}; events: {:?}",
            session_id,
            expected,
            meta.status,
            session_store
                .get_events(session_id, EventRange::all())
                .await
                .expect("events")
                .iter()
                .map(|record| &record.event)
                .collect::<Vec<_>>()
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_approval_request(
    session_store: &PostgresSessionStore,
    session_id: moa_core::SessionId,
) -> moa_core::EventRecord {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let events = session_store
            .get_events(session_id, EventRange::all())
            .await
            .expect("events");
        if let Some(event) = events
            .iter()
            .find(|record| matches!(record.event, Event::ApprovalRequested { .. }))
        {
            return event.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for approval request in session {}",
            session_id
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_successful_tool_result(
    session_store: &PostgresSessionStore,
    session_id: moa_core::SessionId,
) -> ToolOutput {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let events = session_store
            .get_events(session_id, EventRange::all())
            .await
            .expect("events");
        if let Some(output) = events.iter().find_map(|record| match &record.event {
            Event::ToolResult {
                success: true,
                output,
                ..
            } => Some(output.clone()),
            _ => None,
        }) {
            return output;
        }
        if let Some(record) = events
            .iter()
            .find(|record| matches!(record.event, Event::ToolError { .. }))
        {
            panic!(
                "tool execution failed for session {session_id}: {:?}",
                record.event
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for successful tool result in session {session_id}; events: {:?}",
            events
                .iter()
                .map(|record| &record.event)
                .collect::<Vec<_>>()
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_final_response(
    session_store: &PostgresSessionStore,
    session_id: moa_core::SessionId,
    token: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let events = session_store
            .get_events(session_id, EventRange::all())
            .await
            .expect("events");
        if events.iter().any(|record| {
            matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text.contains(token)
            )
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for final response token {token} in session {session_id}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn run_live_provider_tool_approval_roundtrip(provider: LiveProvider) {
    let label = provider.label.to_string();
    let model = provider.model;
    let token = format!("LIVE-E2E-{}", label.to_uppercase());
    let (_dir, session_store, orchestrator) = live_orchestrator_with_provider(provider.provider)
        .await
        .unwrap_or_else(|error| panic!("{label} orchestrator setup failed: {error}"));

    let relative_path = format!("live/{label}.txt");
    let prompt = format!(
        "Use the file_write tool exactly once to write \"{token}\" to \"{relative_path}\". \
         After the tool succeeds, answer with exactly {token}."
    );
    let session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new(format!("ws-{label}")),
            user_id: UserId::new(format!("u-{label}")),
            platform: Platform::Cli,
            model: model.into(),
            initial_message: Some(UserMessage {
                text: prompt,
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .unwrap_or_else(|error| panic!("{label} start session failed: {error}"));

    wait_for_status(
        &orchestrator,
        &session_store,
        session.session_id,
        SessionStatus::WaitingApproval,
    )
    .await;
    let approval = wait_for_approval_request(&session_store, session.session_id).await;
    let request_id = match approval.event {
        Event::ApprovalRequested { request_id, .. } => request_id,
        _ => unreachable!("approval helper returned non-approval event"),
    };

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: ApprovalDecision::AllowOnce,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{label} approval signal failed: {error}"));

    let tool_output = wait_for_successful_tool_result(&session_store, session.session_id).await;
    assert!(
        tool_output.to_text().contains(&relative_path),
        "{label} returned an unexpected tool output: {:?}",
        tool_output
    );
    wait_for_status(
        &orchestrator,
        &session_store,
        session.session_id,
        SessionStatus::Completed,
    )
    .await;
    wait_for_final_response(&session_store, session.session_id, &token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual live provider orchestrator test"]
async fn live_providers_complete_tool_approval_roundtrip_when_available() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        run_live_provider_tool_approval_roundtrip(provider).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual live Gemini orchestrator test"]
async fn live_google_provider_complete_tool_approval_roundtrip_when_available() {
    let Some(provider) = google_live_provider() else {
        return;
    };

    run_live_provider_tool_approval_roundtrip(provider).await;
}
