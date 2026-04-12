//! Tauri command handlers that bridge the frontend to the shared chat runtime.

use moa_core::{ApprovalDecision, PageType, Platform, RuntimeEvent, SessionId, WorkspaceId};
use moa_runtime::ChatRuntime;
use tauri::{State, ipc::Channel};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::AppState;
use crate::dto::{
    EventRecordDto, MemorySearchResultDto, MoaConfigDto, ModelOptionDto, PageSummaryDto,
    RuntimeInfoDto, SessionMetaDto, SessionPreviewDto, SessionSummaryDto, WikiPageDto,
};
use crate::error::{AppResult, MoaAppError};
use crate::stream::StreamEvent;

/// Creates a new session and switches the desktop runtime to it.
#[tauri::command]
pub async fn create_session(state: State<'_, AppState>) -> AppResult<String> {
    let runtime = clone_runtime(&state).await;
    let session_id = runtime.create_session().await?;
    let next_runtime = attach_runtime(&runtime, session_id.clone()).await?;
    replace_runtime(&state, next_runtime).await;
    Ok(session_id.to_string())
}

/// Switches the desktop runtime to an existing session.
#[tauri::command]
pub async fn select_session(
    session_id: String,
    state: State<'_, AppState>,
) -> AppResult<SessionMetaDto> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    let next_runtime = attach_runtime(&runtime, session_id.clone()).await?;
    let meta = next_runtime.session_meta_by_id(session_id).await?;
    replace_runtime(&state, next_runtime).await;
    Ok(meta.into())
}

/// Lists sessions for the active workspace and current user.
#[tauri::command]
pub async fn list_sessions(state: State<'_, AppState>) -> AppResult<Vec<SessionSummaryDto>> {
    let runtime = clone_runtime(&state).await;
    let active_session_id = runtime.session_id().to_string();
    let sessions = runtime.list_sessions().await?;
    Ok(sessions
        .into_iter()
        .map(|session| SessionSummaryDto::from_summary(session, &active_session_id))
        .collect())
}

/// Lists session previews with last-message snippets for the sidebar.
#[tauri::command]
pub async fn list_session_previews(
    state: State<'_, AppState>,
) -> AppResult<Vec<SessionPreviewDto>> {
    let runtime = clone_runtime(&state).await;
    let active_session_id = runtime.session_id().to_string();
    let previews = runtime.list_session_previews().await?;
    Ok(previews
        .into_iter()
        .map(|preview| SessionPreviewDto::from_preview(preview, &active_session_id))
        .collect())
}

/// Loads one session metadata snapshot by identifier.
#[tauri::command]
pub async fn get_session(
    session_id: String,
    state: State<'_, AppState>,
) -> AppResult<SessionMetaDto> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    let session = runtime.session_meta_by_id(session_id).await?;
    Ok(session.into())
}

/// Loads the full persisted event log for one session.
#[tauri::command]
pub async fn get_session_events(
    session_id: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<EventRecordDto>> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    let events = runtime.session_events(session_id).await?;
    Ok(events.into_iter().map(EventRecordDto::from).collect())
}

/// Returns the currently selected runtime info snapshot.
#[tauri::command]
pub async fn get_runtime_info(state: State<'_, AppState>) -> AppResult<RuntimeInfoDto> {
    let runtime = clone_runtime(&state).await;
    Ok(RuntimeInfoDto::from_runtime(&runtime))
}

/// Changes the active workspace and starts a fresh session there.
#[tauri::command]
pub async fn set_workspace(workspace_id: String, state: State<'_, AppState>) -> AppResult<String> {
    let mut runtime = clone_runtime(&state).await;
    let session_id = runtime
        .set_workspace(WorkspaceId::new(workspace_id))
        .await?;
    replace_runtime(&state, runtime).await;
    Ok(session_id.to_string())
}

/// Replaces the active session with a fresh empty session.
#[tauri::command]
pub async fn reset_session(state: State<'_, AppState>) -> AppResult<String> {
    let mut runtime = clone_runtime(&state).await;
    let session_id = runtime.reset_session().await?;
    replace_runtime(&state, runtime).await;
    Ok(session_id.to_string())
}

/// Switches the runtime to a different model and starts a fresh session.
#[tauri::command]
pub async fn set_model(model: String, state: State<'_, AppState>) -> AppResult<String> {
    let mut runtime = clone_runtime(&state).await;
    let session_id = runtime.set_model(model).await?;
    replace_runtime(&state, runtime).await;
    Ok(session_id.to_string())
}

/// Lists curated model options for the desktop model selector.
#[tauri::command]
pub async fn list_model_options(state: State<'_, AppState>) -> AppResult<Vec<ModelOptionDto>> {
    let runtime = clone_runtime(&state).await;
    let config = runtime.config();
    let current_model = runtime.model().to_string();
    let current_provider = config.general.default_provider.clone();

    let mut options = vec![
        ModelOptionDto {
            value: "gpt-5.4".to_string(),
            label: "GPT-5.4".to_string(),
            provider: "openai".to_string(),
        },
        ModelOptionDto {
            value: "gpt-5.4-mini".to_string(),
            label: "GPT-5.4 Mini".to_string(),
            provider: "openai".to_string(),
        },
        ModelOptionDto {
            value: "gpt-5.4-nano".to_string(),
            label: "GPT-5.4 Nano".to_string(),
            provider: "openai".to_string(),
        },
        ModelOptionDto {
            value: "claude-sonnet-4-6".to_string(),
            label: "Claude Sonnet 4.6".to_string(),
            provider: "anthropic".to_string(),
        },
        ModelOptionDto {
            value: "openrouter:gpt-5.4".to_string(),
            label: "OpenRouter · GPT-5.4".to_string(),
            provider: "openrouter".to_string(),
        },
        ModelOptionDto {
            value: "openrouter:claude-sonnet-4-6".to_string(),
            label: "OpenRouter · Claude Sonnet 4.6".to_string(),
            provider: "openrouter".to_string(),
        },
    ];

    if !options.iter().any(|option| option.value == current_model) {
        options.push(ModelOptionDto {
            value: current_model.clone(),
            label: current_model.clone(),
            provider: current_provider,
        });
    }

    options.sort_by_key(|option| {
        let is_current = option.value != current_model;
        (is_current, option.label.clone())
    });

    Ok(options)
}

/// Lists the currently available tool names.
#[tauri::command]
pub async fn get_tool_names(state: State<'_, AppState>) -> AppResult<Vec<String>> {
    let runtime = clone_runtime(&state).await;
    runtime.tool_names_async().await.map_err(Into::into)
}

/// Queues a prompt on an existing session without streaming results.
#[tauri::command]
pub async fn queue_message(
    session_id: String,
    prompt: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    runtime.queue_message(session_id, prompt).await?;
    Ok(())
}

/// Runs one streamed turn on the selected session and forwards runtime events to the frontend.
#[tauri::command]
pub async fn send_message(
    session_id: String,
    prompt: String,
    on_event: Channel<StreamEvent>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let requested_session_id = parse_session_id(&session_id)?;
    let mut runtime = clone_runtime(&state).await;
    if runtime.session_id() != &requested_session_id {
        runtime = attach_runtime(&runtime, requested_session_id).await?;
        replace_runtime(&state, runtime.clone()).await;
    }

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let prompt_clone = prompt.clone();
    let runtime_clone = runtime.clone();
    let run_task =
        tauri::async_runtime::spawn(
            async move { runtime_clone.run_turn(prompt_clone, event_tx).await },
        );

    while let Some(event) = event_rx.recv().await {
        let should_stop = matches!(event, RuntimeEvent::TurnCompleted | RuntimeEvent::Error(_));
        if on_event.send(StreamEvent::from(event)).is_err() {
            break;
        }
        if should_stop {
            break;
        }
    }

    match run_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            let _ = on_event.send(StreamEvent::Error {
                message: error.to_string(),
            });
            Err(error.into())
        }
        Err(error) => {
            let message = format!("stream task failed to join: {error}");
            let _ = on_event.send(StreamEvent::Error {
                message: message.clone(),
            });
            Err(MoaAppError::Internal(message))
        }
    }
}

/// Sends an immediate stop request to the target session.
#[tauri::command]
pub async fn stop_session(session_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    runtime.hard_cancel_session(session_id).await?;
    Ok(())
}

/// Sends a graceful soft-cancel request to the target session.
#[tauri::command]
pub async fn soft_cancel_session(session_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    runtime.soft_cancel_session(session_id).await?;
    Ok(())
}

/// Sends an immediate hard-cancel request to the target session.
#[tauri::command]
pub async fn hard_cancel_session(session_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    runtime.hard_cancel_session(session_id).await?;
    Ok(())
}

/// Responds to an approval request on the active session.
#[tauri::command]
pub async fn respond_to_approval(
    request_id: String,
    decision: String,
    pattern: Option<String>,
    reason: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    let request_id = parse_uuid(&request_id)?;
    let decision = parse_approval_decision(&decision, pattern, reason)?;
    runtime.respond_to_approval(request_id, decision).await?;
    Ok(())
}

/// Responds to an approval request on an explicitly selected session.
#[tauri::command]
pub async fn respond_to_session_approval(
    session_id: String,
    request_id: String,
    decision: String,
    pattern: Option<String>,
    reason: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    let session_id = parse_session_id(&session_id)?;
    let request_id = parse_uuid(&request_id)?;
    let decision = parse_approval_decision(&decision, pattern, reason)?;
    runtime
        .respond_to_session_approval(session_id, request_id, decision)
        .await?;
    Ok(())
}

/// Cancels the currently active generation immediately.
#[tauri::command]
pub async fn cancel_active_generation(state: State<'_, AppState>) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    runtime.cancel_active_generation().await?;
    Ok(())
}

/// Lists memory pages in the active workspace.
#[tauri::command]
pub async fn list_memory_pages(
    filter: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<PageSummaryDto>> {
    let runtime = clone_runtime(&state).await;
    let filter = parse_page_type(filter)?;
    let pages = runtime.list_memory_pages(filter).await?;
    Ok(pages.into_iter().map(PageSummaryDto::from).collect())
}

/// Returns the most recently updated memory pages for the sidebar.
#[tauri::command]
pub async fn recent_memory_entries(
    limit: Option<usize>,
    state: State<'_, AppState>,
) -> AppResult<Vec<PageSummaryDto>> {
    let runtime = clone_runtime(&state).await;
    let pages = runtime.recent_memory_entries(limit.unwrap_or(20)).await?;
    Ok(pages.into_iter().map(PageSummaryDto::from).collect())
}

/// Loads one wiki page from the active workspace.
#[tauri::command]
pub async fn read_memory_page(path: String, state: State<'_, AppState>) -> AppResult<WikiPageDto> {
    let runtime = clone_runtime(&state).await;
    let page = runtime
        .read_memory_page(&moa_core::MemoryPath::new(path))
        .await?;
    Ok(page.into())
}

/// Searches memory in the active workspace.
#[tauri::command]
pub async fn search_memory(
    query: String,
    limit: Option<usize>,
    state: State<'_, AppState>,
) -> AppResult<Vec<MemorySearchResultDto>> {
    let runtime = clone_runtime(&state).await;
    let results = runtime.search_memory(&query, limit.unwrap_or(10)).await?;
    Ok(results
        .into_iter()
        .map(MemorySearchResultDto::from)
        .collect())
}

/// Deletes one wiki page from the active workspace.
#[tauri::command]
pub async fn delete_memory_page(path: String, state: State<'_, AppState>) -> AppResult<()> {
    let runtime = clone_runtime(&state).await;
    runtime
        .delete_memory_page(&moa_core::MemoryPath::new(path))
        .await?;
    Ok(())
}

/// Returns the current workspace memory index document.
#[tauri::command]
pub async fn memory_index(state: State<'_, AppState>) -> AppResult<String> {
    let runtime = clone_runtime(&state).await;
    runtime.memory_index().await.map_err(Into::into)
}

/// Returns the current runtime configuration snapshot.
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> AppResult<MoaConfigDto> {
    let runtime = clone_runtime(&state).await;
    Ok(MoaConfigDto::from(runtime.config()))
}

async fn clone_runtime(state: &State<'_, AppState>) -> ChatRuntime {
    state.runtime.lock().await.clone()
}

async fn replace_runtime(state: &State<'_, AppState>, runtime: ChatRuntime) {
    *state.runtime.lock().await = runtime;
}

async fn attach_runtime(runtime: &ChatRuntime, session_id: SessionId) -> AppResult<ChatRuntime> {
    let config = runtime.config().clone();
    match runtime {
        ChatRuntime::Local(_) => {
            ChatRuntime::attach_to_local_session(config, Platform::Desktop, session_id)
                .await
                .map_err(Into::into)
        }
        ChatRuntime::Daemon(_) => {
            ChatRuntime::attach_to_daemon_session(config, Platform::Desktop, session_id)
                .await
                .map_err(Into::into)
        }
    }
}

fn parse_session_id(value: &str) -> AppResult<SessionId> {
    Ok(SessionId(Uuid::parse_str(value)?))
}

fn parse_uuid(value: &str) -> AppResult<Uuid> {
    Uuid::parse_str(value).map_err(Into::into)
}

fn parse_page_type(value: Option<String>) -> AppResult<Option<PageType>> {
    value
        .as_deref()
        .map(|value| match value {
            "index" => Ok(PageType::Index),
            "topic" => Ok(PageType::Topic),
            "entity" => Ok(PageType::Entity),
            "decision" => Ok(PageType::Decision),
            "skill" => Ok(PageType::Skill),
            "source" => Ok(PageType::Source),
            "schema" => Ok(PageType::Schema),
            "log" => Ok(PageType::Log),
            other => Err(MoaAppError::InvalidInput(format!(
                "unknown page type `{other}`"
            ))),
        })
        .transpose()
}

fn parse_approval_decision(
    decision: &str,
    pattern: Option<String>,
    reason: Option<String>,
) -> AppResult<ApprovalDecision> {
    match decision {
        "allow_once" => Ok(ApprovalDecision::AllowOnce),
        "always_allow" => Ok(ApprovalDecision::AlwaysAllow {
            pattern: pattern.unwrap_or_else(|| "*".to_string()),
        }),
        "deny" => Ok(ApprovalDecision::Deny { reason }),
        other => Err(MoaAppError::InvalidInput(format!(
            "unknown approval decision `{other}`"
        ))),
    }
}
