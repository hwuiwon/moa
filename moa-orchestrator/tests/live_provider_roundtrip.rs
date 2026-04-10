use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, Event, EventRange, LLMProvider, MoaConfig, Platform,
    Result, SessionSignal, SessionStatus, SessionStore, StartSessionRequest, UserId, UserMessage,
    WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::LocalOrchestrator;
use moa_providers::{AnthropicProvider, OpenAIProvider, OpenRouterProvider};
use moa_session::TursoSessionStore;
use tempfile::TempDir;
use tokio::time::{Instant, sleep};

enum LiveProvider {
    OpenAi(OpenAIProvider, &'static str),
    Anthropic(AnthropicProvider, &'static str),
    OpenRouter(OpenRouterProvider, &'static str),
}

impl LiveProvider {
    fn label(&self) -> &'static str {
        match self {
            Self::OpenAi(_, label) | Self::Anthropic(_, label) | Self::OpenRouter(_, label) => {
                label
            }
        }
    }

    fn model(&self) -> String {
        match self {
            Self::OpenAi(provider, _) => provider.capabilities().model_id,
            Self::Anthropic(provider, _) => provider.capabilities().model_id,
            Self::OpenRouter(provider, _) => provider.capabilities().model_id,
        }
    }

    fn into_arc(self) -> Arc<dyn LLMProvider> {
        match self {
            Self::OpenAi(provider, _) => Arc::new(provider),
            Self::Anthropic(provider, _) => Arc::new(provider),
            Self::OpenRouter(provider, _) => Arc::new(provider),
        }
    }
}

fn available_live_providers() -> Vec<LiveProvider> {
    let mut providers = Vec::new();
    if let Ok(provider) = OpenAIProvider::from_env("gpt-5.4") {
        providers.push(LiveProvider::OpenAi(provider, "openai"));
    }
    if let Ok(provider) = AnthropicProvider::from_env("claude-sonnet-4-6") {
        providers.push(LiveProvider::Anthropic(provider, "anthropic"));
    }
    if let Ok(provider) = OpenRouterProvider::from_env("openai/gpt-5.4") {
        providers.push(LiveProvider::OpenRouter(provider, "openrouter"));
    }
    providers
}

async fn live_orchestrator_with_provider(
    provider: Arc<dyn LLMProvider>,
) -> Result<(TempDir, Arc<TursoSessionStore>, LocalOrchestrator)> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.local.session_db = dir.path().join("sessions.db").display().to_string();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let session_store = Arc::new(TursoSessionStore::new(&config.local.session_db).await?);
    let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
    let tool_router = Arc::new(
        ToolRouter::from_config(&config, memory_store.clone())
            .await?
            .with_rule_store(session_store.clone()),
    );
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store.clone(),
        memory_store,
        provider,
        tool_router,
    )
    .await?;
    Ok((dir, session_store, orchestrator))
}

async fn wait_for_status(
    orchestrator: &LocalOrchestrator,
    session_id: moa_core::SessionId,
    expected: SessionStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let meta = orchestrator
            .get_session(session_id.clone())
            .await
            .expect("session metadata");
        if meta.status == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for session {} status {:?}; current status {:?}",
            session_id,
            expected,
            meta.status
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_approval_request(
    session_store: &TursoSessionStore,
    session_id: moa_core::SessionId,
) -> moa_core::EventRecord {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let events = session_store
            .get_events(session_id.clone(), EventRange::all())
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

async fn wait_for_file(root: PathBuf, relative: &str) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(mut sandboxes) = tokio::fs::read_dir(&root).await {
            while let Ok(Some(entry)) = sandboxes.next_entry().await {
                let candidate = entry.path().join(relative);
                if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
                    return candidate;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for sandbox file {} beneath {}",
            relative,
            root.display()
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_final_response(
    session_store: &TursoSessionStore,
    session_id: moa_core::SessionId,
    token: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let events = session_store
            .get_events(session_id.clone(), EventRange::all())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual live provider orchestrator test"]
async fn live_providers_complete_tool_approval_roundtrip_when_available() {
    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        let label = provider.label().to_string();
        let model = provider.model();
        let token = format!("LIVE-E2E-{}", label.to_uppercase());
        let (dir, session_store, orchestrator) =
            live_orchestrator_with_provider(provider.into_arc())
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
                model,
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
            session.session_id.clone(),
            SessionStatus::WaitingApproval,
        )
        .await;
        let approval = wait_for_approval_request(&session_store, session.session_id.clone()).await;
        let request_id = match approval.event {
            Event::ApprovalRequested { request_id, .. } => request_id,
            _ => unreachable!("approval helper returned non-approval event"),
        };

        orchestrator
            .signal(
                session.session_id.clone(),
                SessionSignal::ApprovalDecided {
                    request_id,
                    decision: ApprovalDecision::AllowOnce,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{label} approval signal failed: {error}"));

        wait_for_status(
            &orchestrator,
            session.session_id.clone(),
            SessionStatus::Completed,
        )
        .await;
        let written = wait_for_file(dir.path().join("sandbox"), &relative_path).await;
        let contents = tokio::fs::read_to_string(&written)
            .await
            .unwrap_or_else(|error| panic!("{label} failed reading written file: {error}"));
        assert_eq!(contents, token, "{label} wrote unexpected file contents");
        wait_for_final_response(&session_store, session.session_id, &token).await;
    }
}
