//! Shared local session task helpers.

use crate::*;

pub(super) async fn accept_user_message(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: SessionId,
    message: UserMessage,
    queued: bool,
) -> Result<()> {
    let event = if queued {
        Event::QueuedMessage {
            text: message.text,
            queued_at: Utc::now(),
        }
    } else {
        Event::UserMessage {
            text: message.text,
            attachments: message.attachments,
        }
    };
    append_event(session_store, event_tx, session_id, event).await?;
    Ok(())
}

pub(super) async fn flush_queued_messages(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: SessionId,
    queued_messages: &mut Vec<BufferedUserMessage>,
) -> Result<()> {
    for message in queued_messages.drain(..) {
        flush_pending_signal(session_store, event_tx, session_id, message).await?;
    }

    Ok(())
}

pub(super) async fn flush_next_queued_message(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: SessionId,
    queued_messages: &mut Vec<BufferedUserMessage>,
) -> Result<bool> {
    if queued_messages.is_empty() {
        return Ok(false);
    }

    let message = queued_messages.remove(0);
    flush_pending_signal(session_store, event_tx, session_id, message).await?;
    Ok(true)
}

pub(super) async fn flush_pending_signal(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: SessionId,
    buffered: BufferedUserMessage,
) -> Result<()> {
    accept_user_message(
        session_store,
        event_tx,
        session_id,
        buffered.message.clone(),
        true,
    )
    .await?;

    if let Some(signal_id) = buffered.pending_signal_id {
        best_effort_resolve_pending_signal(session_store, session_id, signal_id).await?;
        return Ok(());
    }

    if let Some(signal_id) =
        resolve_matching_pending_signal(session_store, session_id, buffered.message.clone()).await?
    {
        best_effort_resolve_pending_signal(session_store, session_id, signal_id).await?;
    } else {
        tracing::warn!(
            session_id = %session_id,
            text = %buffered.message.text,
            "queued message did not have a matching durable pending signal"
        );
    }
    Ok(())
}

pub(super) async fn best_effort_resolve_pending_signal(
    session_store: &Arc<dyn SessionStore>,
    session_id: SessionId,
    signal_id: moa_core::PendingSignalId,
) -> Result<()> {
    match session_store.resolve_pending_signal(signal_id).await {
        Ok(()) => Ok(()),
        Err(MoaError::StorageError(message)) => {
            tracing::warn!(
                session_id = %session_id,
                signal_id = %signal_id,
                error = %message,
                "pending signal was already resolved before flush completed"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn resolve_matching_pending_signal(
    session_store: &Arc<dyn SessionStore>,
    session_id: SessionId,
    message: UserMessage,
) -> Result<Option<moa_core::PendingSignalId>> {
    let pending = session_store.get_pending_signals(session_id).await?;
    for signal in pending {
        if signal.user_message()? == message {
            return Ok(Some(signal.id));
        }
    }
    Ok(None)
}

pub(super) async fn update_status(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    next_status: SessionStatus,
) -> Result<()> {
    let previous_status = status.read().await.clone();
    if previous_status == next_status {
        return Ok(());
    }
    if let Some(record) = session_store
        .transition_status(session_id, next_status.clone())
        .await?
    {
        let _ = event_tx.send(record);
    }
    *status.write().await = next_status;
    Ok(())
}

pub(super) async fn refresh_workspace_tool_stats(
    session_store: &Arc<dyn SessionStore>,
    session_id: SessionId,
) {
    let _ = session_store;
    tracing::debug!(session_id = %session_id, "skipped workspace tool stats refresh");
}

pub(super) async fn append_event(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: SessionId,
    event: Event,
) -> Result<EventRecord> {
    let sequence_num = session_store.emit_event(session_id, event).await?;
    let mut records = session_store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(sequence_num),
                to_seq: Some(sequence_num),
                event_types: None,
                limit: Some(1),
            },
        )
        .await?;
    let record = records
        .pop()
        .ok_or_else(|| MoaError::StorageError("failed to reload appended event".to_string()))?;
    let _ = event_tx.send(record.clone());
    Ok(record)
}

pub(super) async fn detect_workspace_path(workspace_id: &WorkspaceId) -> Result<PathBuf> {
    let cwd = env::current_dir().map_err(|error| {
        MoaError::ProviderError(format!("failed to resolve current directory: {error}"))
    })?;
    let cwd = match cwd.canonicalize() {
        Ok(path) => path,
        Err(_) => cwd,
    };

    for candidate in cwd.ancestors() {
        let git_dir = candidate.join(".git");
        if tokio::fs::try_exists(&git_dir).await? {
            return Ok(candidate.to_path_buf());
        }
    }

    if cwd
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == workspace_id.as_str())
        .unwrap_or(false)
    {
        return Ok(cwd);
    }

    let candidate = cwd.join(workspace_id.as_str());
    if tokio::fs::try_exists(&candidate).await? {
        return Ok(candidate);
    }

    Ok(cwd)
}

pub(super) async fn workspace_has_graph_nodes(
    pool: &sqlx::PgPool,
    workspace_id: &WorkspaceId,
) -> Result<bool> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM moa.node_index
        WHERE workspace_id = $1
          AND valid_to IS NULL
        "#,
    )
    .bind(workspace_id.as_str())
    .fetch_one(pool)
    .await
    .map_err(|error| MoaError::StorageError(error.to_string()))?;
    Ok(count > 0)
}

pub(super) async fn report_session_task_failure(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    message: String,
) -> Result<()> {
    let current = session_store.get_session(session_id).await?;
    if matches!(current.status, SessionStatus::Failed) {
        return Ok(());
    }

    append_event(
        session_store,
        event_tx,
        session_id,
        Event::Error {
            message,
            recoverable: false,
        },
    )
    .await?;
    update_status(
        session_store,
        event_tx,
        status,
        session_id,
        SessionStatus::Failed,
    )
    .await
}

/// Reports a recoverable session-task error: writes a `Warning` event and
/// parks the session at `Paused` so the UI can offer a Resume affordance
/// rather than treating it as terminal.
pub(super) async fn report_session_task_paused(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    message: String,
) -> Result<()> {
    let current = session_store.get_session(session_id).await?;
    if matches!(
        current.status,
        SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Completed
    ) {
        return Ok(());
    }

    append_event(
        session_store,
        event_tx,
        session_id,
        Event::Warning { message },
    )
    .await?;
    update_status(
        session_store,
        event_tx,
        status,
        session_id,
        SessionStatus::Paused,
    )
    .await
}

pub(super) async fn pause_active_session(
    context: &SessionTaskContext,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    queued_messages: &mut Vec<BufferedUserMessage>,
    message: String,
) -> Result<()> {
    flush_queued_messages(
        &context.session_store,
        event_tx,
        session_id,
        queued_messages,
    )
    .await?;
    refresh_workspace_tool_stats(&context.session_store, session_id).await;
    pause_session_task(
        &context.session_store,
        event_tx,
        status,
        session_id,
        message.clone(),
    )
    .await?;
    context.tool_router.destroy_session_hands(&session_id).await;
    let _ = runtime_tx.send(RuntimeEvent::Notice(message));
    if let Err(err) = runtime_tx.send(RuntimeEvent::TurnCompleted) {
        tracing::warn!(
            ?err,
            "runtime receiver dropped while sending TurnCompleted (pause)"
        );
    }
    Ok(())
}

pub(super) async fn pause_session_task(
    session_store: &Arc<dyn SessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    message: String,
) -> Result<()> {
    let current = session_store.get_session(session_id).await?;
    if matches!(
        current.status,
        SessionStatus::Failed | SessionStatus::Cancelled
    ) {
        return Ok(());
    }

    append_event(
        session_store,
        event_tx,
        session_id,
        Event::Warning { message },
    )
    .await?;
    update_status(
        session_store,
        event_tx,
        status,
        session_id,
        SessionStatus::Paused,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn record_turn_boundary(
    context: &SessionTaskContext,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    queued_messages: &mut Vec<BufferedUserMessage>,
    turn_start_sequence_num: u64,
    turn_count: &mut u32,
    loop_detector: &mut LoopDetector,
    loop_detection_threshold: u32,
) -> Result<bool> {
    *turn_count = turn_count.saturating_add(1);
    let completed_turn_events = context
        .session_store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(turn_start_sequence_num.saturating_add(1)),
                ..EventRange::default()
            },
        )
        .await?;
    let tool_summaries = collect_turn_tool_summaries(&completed_turn_events);
    if tool_summaries.is_empty() || !loop_detector.record_turn(&tool_summaries) {
        return Ok(false);
    }

    let updated_events = context
        .session_store
        .get_events(session_id, EventRange::all())
        .await?;
    pause_active_session(
        context,
        event_tx,
        runtime_tx,
        status,
        session_id,
        queued_messages,
        loop_detected_pause_message(loop_detection_threshold, &updated_events),
    )
    .await?;
    Ok(true)
}

pub(super) fn last_user_message_text(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

pub(super) fn collect_turn_tool_summaries(events: &[EventRecord]) -> Vec<(String, String)> {
    let mut tool_calls = Vec::new();
    let mut outputs = HashMap::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id, tool_name, ..
            } => tool_calls.push((*tool_id, tool_name.clone())),
            Event::ToolResult {
                tool_id, output, ..
            } => {
                outputs.insert(*tool_id, truncate_loop_output(&output.to_text()));
            }
            Event::ToolError { tool_id, error, .. } => {
                outputs.insert(*tool_id, truncate_loop_output(error));
            }
            _ => {}
        }
    }

    tool_calls
        .into_iter()
        .map(|(tool_id, tool_name)| {
            let output = outputs.remove(&tool_id).unwrap_or_default();
            (tool_name, output)
        })
        .collect()
}

pub(super) fn turn_limit_pause_message(turn_count: u32, events: &[EventRecord]) -> String {
    let noun = if turn_count == 1 { "turn" } else { "turns" };
    let base = format!("Session paused after {turn_count} {noun}. Use /resume to continue.");
    append_pause_summary(base, events)
}

pub(super) fn loop_detected_pause_message(threshold: u32, events: &[EventRecord]) -> String {
    let noun = if threshold == 1 { "turn" } else { "turns" };
    let base = format!(
        "Loop detected after {threshold} consecutive {noun} with identical tool call patterns. Session paused. Use /resume to continue."
    );
    append_pause_summary(base, events)
}

pub(super) fn append_pause_summary(base: String, events: &[EventRecord]) -> String {
    let Some(summary) = latest_brain_response_summary(events) else {
        return base;
    };
    format!("{base} Latest assistant response: {summary}")
}

pub(super) fn latest_brain_response_summary(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } => {
            let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
            if line.is_empty() {
                None
            } else {
                Some(truncate_loop_output(line))
            }
        }
        _ => None,
    })
}

pub(super) fn truncate_loop_output(value: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut iter = value.chars();
    let mut buf: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        buf.push_str("...");
    }
    buf
}
