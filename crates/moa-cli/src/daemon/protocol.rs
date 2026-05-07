//! Daemon socket protocol handlers.

use super::*;

pub(super) async fn handle_connection(
    state: DaemonState,
    shutdown_tx: watch::Sender<bool>,
    stream: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }

    let command: DaemonCommand = serde_json::from_str(line.trim_end())?;
    match command {
        DaemonCommand::ObserveSession { session_id } => {
            write_stream_event(reader.get_mut(), &DaemonStreamEvent::Ready).await?;
            let receiver: tokio::sync::broadcast::Receiver<RuntimeEvent> = state
                .orchestrator
                .observe_runtime(session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("runtime observation is unavailable"))?;
            relay_runtime_stream(session_id, reader.get_mut(), receiver).await?;
            Ok(())
        }
        other => {
            let reply = handle_unary_command(state, shutdown_tx, other).await;
            write_reply(reader.get_mut(), &reply).await
        }
    }
}

async fn relay_runtime_stream(
    session_id: SessionId,
    stream: &mut UnixStream,
    mut receiver: tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Result<()> {
    loop {
        match recv_with_lag_handling(
            &mut receiver,
            BroadcastChannel::Runtime,
            &session_id,
            LagPolicy::SkipWithGap,
        )
        .await
        {
            RecvResult::Message(event) => {
                write_stream_event(stream, &DaemonStreamEvent::Runtime(event)).await?;
            }
            RecvResult::Gap { count } | RecvResult::BackfillRequested { count } => {
                write_stream_event(
                    stream,
                    &DaemonStreamEvent::Gap {
                        count,
                        channel: BroadcastChannel::Runtime,
                    },
                )
                .await?;
            }
            RecvResult::AbortRequested | RecvResult::Closed => return Ok(()),
        }
    }
}

async fn handle_unary_command(
    state: DaemonState,
    shutdown_tx: watch::Sender<bool>,
    command: DaemonCommand,
) -> DaemonReply {
    match handle_unary_command_inner(state, shutdown_tx, command).await {
        Ok(reply) => reply,
        Err(error) => DaemonReply::Error(error.to_string()),
    }
}

async fn handle_unary_command_inner(
    state: DaemonState,
    shutdown_tx: watch::Sender<bool>,
    command: DaemonCommand,
) -> Result<DaemonReply> {
    match command {
        DaemonCommand::Ping => {
            let sessions = state
                .session_store
                .list_sessions(SessionFilter::default())
                .await?;
            let active_session_count = sessions
                .iter()
                .filter(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Created
                            | SessionStatus::Running
                            | SessionStatus::WaitingApproval
                    )
                })
                .count();
            let mut info = (*state.info).clone();
            info.session_count = sessions.len();
            info.active_session_count = active_session_count;
            Ok(DaemonReply::Info(info))
        }
        DaemonCommand::Shutdown => {
            let _ = shutdown_tx.send(true);
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::CreateSession { request } => {
            let handle = state.orchestrator.start_session(request).await?;
            Ok(DaemonReply::SessionId(handle.session_id))
        }
        DaemonCommand::SetWorkspace { .. } => Ok(DaemonReply::Ack),
        DaemonCommand::SetModel { .. } => Ok(DaemonReply::Ack),
        DaemonCommand::ListSessions { filter } => Ok(DaemonReply::Sessions(
            state.session_store.list_sessions(filter).await?,
        )),
        DaemonCommand::ListSessionPreviews { filter } => Ok(DaemonReply::SessionPreviews(
            list_session_previews(state.session_store.as_ref(), filter)
                .await?
                .into_iter()
                .collect(),
        )),
        DaemonCommand::GetSession { session_id } => Ok(DaemonReply::Session(
            state.session_store.get_session(session_id).await?,
        )),
        DaemonCommand::GetSessionEvents { session_id } => Ok(DaemonReply::SessionEvents(
            state
                .session_store
                .get_events(session_id, EventRange::all())
                .await?,
        )),
        DaemonCommand::ToolNames => Ok(DaemonReply::ToolNames(state.orchestrator.tool_names())),
        DaemonCommand::GetWorkspaceBudgetStatus { workspace_id } => {
            let day_start = Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|value| value.and_utc())
                .ok_or_else(|| anyhow::anyhow!("failed to compute UTC day boundary"))?;
            let daily_spent_cents = state
                .session_store
                .workspace_cost_since(&workspace_id, day_start)
                .await?;
            Ok(DaemonReply::WorkspaceBudgetStatus(WorkspaceBudgetStatus {
                daily_budget_cents: state.daily_workspace_budget_cents,
                daily_spent_cents,
            }))
        }
        DaemonCommand::QueueMessage { session_id, prompt } => {
            state
                .orchestrator
                .signal(
                    session_id,
                    moa_core::SessionSignal::QueueMessage(moa_core::UserMessage {
                        text: prompt,
                        attachments: Vec::new(),
                    }),
                )
                .await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::SoftCancel { session_id } => {
            state
                .orchestrator
                .signal(session_id, moa_core::SessionSignal::SoftCancel)
                .await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::HardCancel { session_id } => {
            state
                .orchestrator
                .signal(session_id, moa_core::SessionSignal::HardCancel)
                .await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::RespondToApproval {
            session_id,
            request_id,
            decision,
        } => {
            state
                .orchestrator
                .signal(
                    session_id,
                    moa_core::SessionSignal::ApprovalDecided {
                        request_id,
                        decision,
                    },
                )
                .await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::ObserveSession { .. } => bail!("observe is handled separately"),
    }
}

async fn list_session_previews(
    session_store: &PostgresSessionStore,
    filter: SessionFilter,
) -> Result<Vec<DaemonSessionPreview>> {
    let mut previews = Vec::new();
    for summary in session_store.list_sessions(filter).await? {
        let events = session_store
            .get_events(summary.session_id, EventRange::recent(16))
            .await?;
        previews.push(DaemonSessionPreview {
            summary,
            last_message: last_session_message(&events),
        });
    }

    Ok(previews)
}

fn last_session_message(events: &[moa_core::EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        moa_core::Event::BrainResponse { text, .. } | moa_core::Event::UserMessage { text, .. } => {
            Some(text.trim().to_string())
        }
        moa_core::Event::QueuedMessage { text, .. } => Some(format!("Queued: {}", text.trim())),
        _ => None,
    })
}
