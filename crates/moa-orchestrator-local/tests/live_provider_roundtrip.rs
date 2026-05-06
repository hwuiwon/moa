use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, Event, EventRange, LLMProvider, MoaConfig, Platform,
    Result, SessionSignal, SessionStatus, SessionStore, StartSessionRequest, UserId, UserMessage,
    WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_orchestrator_local::LocalOrchestrator;
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
    if let Ok(provider) = GeminiProvider::from_env(google_live_model()) {
        providers.push(LiveProvider {
            label: "google",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    providers
}

fn google_live_provider() -> Option<LiveProvider> {
    GeminiProvider::from_env(google_live_model())
        .ok()
        .map(|provider| LiveProvider {
            label: "google",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        })
}

fn google_live_model() -> String {
    std::env::var("GOOGLE_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string())
}

async fn live_orchestrator_with_provider(
    provider: Arc<dyn LLMProvider>,
) -> Result<(TempDir, Arc<PostgresSessionStore>, LocalOrchestrator)> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let (session_store, _database_url, _schema_name) =
        testing::create_isolated_test_store().await?;
    let session_store = Arc::new(session_store);
    let tool_router = Arc::new(
        ToolRouter::from_config(&config)
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store.clone(),
        Arc::new(ModelRouter::new(provider, None)),
        tool_router,
    )
    .await?;
    Ok((dir, session_store, orchestrator))
}

async fn approve_matching_requests_until_complete(
    label: &str,
    orchestrator: &LocalOrchestrator,
    session_store: &PostgresSessionStore,
    session_id: moa_core::SessionId,
    relative_path: &str,
    token: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut approved = HashSet::new();
    let mut saw_successful_file_write = false;

    loop {
        let events = session_store
            .get_events(session_id, EventRange::all())
            .await
            .expect("events");

        if let Some(record) = events
            .iter()
            .find(|record| matches!(record.event, Event::ToolError { .. }))
        {
            panic!(
                "{label} tool execution failed for session {session_id}: {:?}",
                record.event
            );
        }

        for record in &events {
            if let Event::ToolResult {
                success: true,
                output,
                ..
            } = &record.event
                && output.to_text().contains(relative_path)
            {
                saw_successful_file_write = true;
            }
        }

        let final_response_seen = events.iter().any(|record| {
            matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text.contains(token)
            )
        });
        let meta = orchestrator
            .get_session(session_id)
            .await
            .expect("session metadata");
        if final_response_seen && meta.status == SessionStatus::Completed {
            assert!(
                saw_successful_file_write,
                "{label} completed without a successful file_write result for {relative_path}"
            );
            return;
        }

        if let Some(request_id) = events.iter().find_map(|record| match &record.event {
            Event::ApprovalRequested {
                request_id,
                tool_name,
                input_summary,
                ..
            } if !approved.contains(request_id)
                && tool_name == "file_write"
                && input_summary.contains(relative_path) =>
            {
                Some(*request_id)
            }
            _ => None,
        }) {
            assert!(
                approved.len() < 4,
                "{label} requested too many repeated approvals in session {session_id}: {:?}",
                events
                    .iter()
                    .map(|record| &record.event)
                    .collect::<Vec<_>>()
            );
            approved.insert(request_id);
            orchestrator
                .signal(
                    session_id,
                    SessionSignal::ApprovalDecided {
                        request_id,
                        decision: ApprovalDecision::AllowOnce,
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{label} approval signal failed: {error}"));
            continue;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for {label} session {session_id} to complete with token {token}; \
             current status {:?}; events: {:?}",
            meta.status,
            events
                .iter()
                .map(|record| &record.event)
                .collect::<Vec<_>>()
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

    approve_matching_requests_until_complete(
        &label,
        &orchestrator,
        &session_store,
        session.session_id,
        &relative_path,
        &token,
    )
    .await;
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
