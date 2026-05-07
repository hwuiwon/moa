//! Concurrent session runner and turn observation logic.

use crate::*;

#[derive(Debug)]
pub(crate) struct TurnObservation {
    pub(crate) latency: Duration,
    pub(crate) ttft: Option<Duration>,
    pub(crate) auto_denied_approvals: usize,
}

pub(crate) async fn run_sessions(
    backend: Arc<dyn SessionTarget>,
    options: &LoadTestOptions,
    plans: Vec<SessionPlan>,
    workspace_root: Option<PathBuf>,
    started: Instant,
) -> Result<LoadTestReport> {
    let (results_tx, mut results_rx) = mpsc::channel(plans.len());
    for plan in plans {
        let backend = backend.clone();
        let results_tx = results_tx.clone();
        let inter_message_delay = options.inter_message_delay;
        let turn_timeout = options.turn_timeout;
        tokio::spawn(async move {
            let report = simulate_session(backend, plan, inter_message_delay, turn_timeout).await;
            let _ = results_tx.send(report).await;
        });
    }
    drop(results_tx);

    let mut sessions = Vec::new();
    while let Some(report) = results_rx.recv().await {
        sessions.push(report);
    }

    sessions.sort_by_key(|session| session.session_id.to_string());
    let sessions_completed = sessions
        .iter()
        .filter(|session| session.failure_reason.is_none())
        .count();
    let sessions_failed = sessions.len().saturating_sub(sessions_completed);
    let error_count = sessions.iter().map(|session| session.error_count).sum();
    let total_tool_calls = sessions.iter().map(|session| session.tool_calls).sum();
    let auto_denied_approvals = sessions
        .iter()
        .map(|session| session.auto_denied_approvals)
        .sum();
    let total_cost_cents = sessions
        .iter()
        .map(|session| session.total_cost_cents)
        .sum();
    let latency_samples = sessions
        .iter()
        .flat_map(|session| session.turn_latency_ms.iter().copied())
        .collect::<Vec<_>>();
    let ttft_samples = sessions
        .iter()
        .flat_map(|session| session.ttft_ms.iter().copied())
        .collect::<Vec<_>>();
    let cache_samples = sessions
        .iter()
        .map(|session| session.cache_hit_rate)
        .collect::<Vec<_>>();

    Ok(LoadTestReport {
        mode: options.mode,
        target: options.target,
        profile: options.profile,
        sessions_requested: options.sessions,
        sessions_completed,
        sessions_failed,
        error_count,
        total_tool_calls,
        auto_denied_approvals,
        duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
        latency_ms: summarize_percentiles(&latency_samples),
        ttft_ms: summarize_percentiles(&ttft_samples),
        cache_hit_rate: summarize_percentiles(&cache_samples),
        total_cost_cents,
        workspace_root,
        sessions,
    })
}

pub(crate) async fn simulate_session(
    backend: Arc<dyn SessionTarget>,
    plan: SessionPlan,
    inter_message_delay: Duration,
    turn_timeout: Duration,
) -> SessionReport {
    let started = Instant::now();
    let session_id = match backend.start_session(&plan).await {
        Ok(session_id) => session_id,
        Err(error) => {
            return SessionReport {
                session_id: SessionId::new(),
                profile: plan.profile,
                status: SessionStatus::Failed,
                planned_turns: plan.turns.len(),
                completed_turns: 0,
                duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
                cache_hit_rate: 0.0,
                total_cost_cents: 0,
                tool_calls: 0,
                error_count: 1,
                auto_denied_approvals: 0,
                turn_latency_ms: Vec::new(),
                ttft_ms: Vec::new(),
                failure_reason: Some(error.to_string()),
            };
        }
    };

    let mut completed_turns = 0usize;
    let mut turn_latency_ms = Vec::new();
    let mut ttft_ms = Vec::new();
    let mut tool_calls = 0usize;
    let mut error_count = 0usize;
    let mut auto_denied_approvals = 0usize;
    let mut last_sequence_num = 0u64;
    let mut failure_reason = None;

    for (turn_index, turn) in plan.turns.iter().enumerate() {
        match backend
            .run_turn(session_id, &turn.prompt, turn_timeout)
            .await
        {
            Ok(observation) => {
                completed_turns += 1;
                turn_latency_ms.push(observation.latency.as_secs_f64() * 1_000.0);
                if let Some(ttft) = observation.ttft {
                    ttft_ms.push(ttft.as_secs_f64() * 1_000.0);
                }
                auto_denied_approvals += observation.auto_denied_approvals;

                match backend.session_events(session_id).await {
                    Ok(events) => {
                        let previous_sequence_num = last_sequence_num;
                        let new_events = events
                            .into_iter()
                            .filter(|record| record.sequence_num > previous_sequence_num)
                            .collect::<Vec<_>>();
                        for record in new_events {
                            last_sequence_num = record.sequence_num;
                            match &record.event {
                                Event::ToolCall { .. } => tool_calls += 1,
                                Event::ToolError { error, .. }
                                    if !is_expected_harness_denial(error) =>
                                {
                                    error_count += 1;
                                }
                                Event::Error { .. } => error_count += 1,
                                _ => {}
                            }
                        }
                    }
                    Err(error) => {
                        failure_reason = Some(format!(
                            "turn {} completed but events could not be loaded: {error}",
                            turn_index + 1
                        ));
                        break;
                    }
                }

                if turn_index + 1 < plan.turns.len() && !inter_message_delay.is_zero() {
                    tokio::time::sleep(inter_message_delay).await;
                }
            }
            Err(error) => {
                failure_reason = Some(format!("turn {} failed: {error}", turn_index + 1));
                break;
            }
        }
    }

    let final_session_note = backend
        .session_events(session_id)
        .await
        .ok()
        .and_then(|events| latest_session_note(&events));

    match backend.session_meta(session_id).await {
        Ok(meta) => {
            let include_session_note = failure_reason.is_some()
                || matches!(
                    meta.status,
                    SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Paused
                );
            SessionReport {
                session_id,
                profile: plan.profile,
                status: meta.status.clone(),
                planned_turns: plan.turns.len(),
                completed_turns,
                duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
                cache_hit_rate: meta.cache_hit_rate(),
                total_cost_cents: meta.total_cost_cents as u64,
                tool_calls,
                error_count,
                auto_denied_approvals,
                turn_latency_ms,
                ttft_ms,
                failure_reason: merge_failure_reason(
                    failure_reason,
                    if matches!(
                        meta.status,
                        SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Paused
                    ) {
                        Some(format!("session ended in status {:?}", meta.status))
                    } else {
                        None
                    },
                    if include_session_note {
                        final_session_note
                    } else {
                        None
                    },
                ),
            }
        }
        Err(error) => SessionReport {
            session_id,
            profile: plan.profile,
            status: SessionStatus::Failed,
            planned_turns: plan.turns.len(),
            completed_turns,
            duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
            cache_hit_rate: 0.0,
            total_cost_cents: 0,
            tool_calls,
            error_count: error_count + 1,
            auto_denied_approvals,
            turn_latency_ms,
            ttft_ms,
            failure_reason: Some(
                merge_failure_reason(
                    failure_reason,
                    Some(format!("failed to load session metadata: {error}")),
                    final_session_note,
                )
                .unwrap_or_else(|| format!("failed to load session metadata: {error}")),
            ),
        },
    }
}

pub(crate) fn latest_session_note(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::Warning { message } => Some(message.clone()),
        Event::Error { message, .. } => Some(message.clone()),
        _ => None,
    })
}

pub(crate) fn is_expected_harness_denial(message: &str) -> bool {
    message.contains("auto-denied by moa-loadtest")
}

pub(crate) fn merge_failure_reason(
    primary: Option<String>,
    secondary: Option<String>,
    note: Option<String>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(primary) = primary {
        parts.push(primary);
    }
    if let Some(secondary) = secondary
        && !parts.iter().any(|existing| existing == &secondary)
    {
        parts.push(secondary);
    }
    if let Some(note) = note
        && !parts.iter().any(|existing| existing == &note)
    {
        parts.push(format!("session note: {note}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

pub(crate) async fn wait_for_turn_completion<Recv, RecvFuture, Approve, ApproveFuture>(
    timeout: Duration,
    mut recv_event: Recv,
    mut respond_to_approval: Approve,
) -> Result<TurnObservation>
where
    Recv: FnMut() -> RecvFuture + Send,
    RecvFuture: std::future::Future<Output = Result<RuntimeEvent>> + Send,
    Approve: FnMut(Uuid) -> ApproveFuture + Send,
    ApproveFuture: std::future::Future<Output = Result<()>> + Send,
{
    let started = Instant::now();
    let deadline = started + timeout;
    let mut ttft = None;
    let mut auto_denied_approvals = 0usize;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(MoaError::ProviderError(format!(
                "turn timed out after {:.2}s",
                timeout.as_secs_f64()
            )));
        }
        let remaining = deadline.saturating_duration_since(now);
        let event = tokio::time::timeout(remaining, recv_event())
            .await
            .map_err(|_| {
                MoaError::ProviderError(format!(
                    "turn timed out after {:.2}s",
                    timeout.as_secs_f64()
                ))
            })??;

        if matches!(
            event,
            RuntimeEvent::AssistantStarted
                | RuntimeEvent::AssistantDelta(_)
                | RuntimeEvent::AssistantFinished { .. }
                | RuntimeEvent::ToolUpdate(_)
                | RuntimeEvent::ApprovalRequested(_)
                | RuntimeEvent::Notice(_)
                | RuntimeEvent::Error(_)
        ) && ttft.is_none()
        {
            ttft = Some(started.elapsed());
        }

        match event {
            RuntimeEvent::ApprovalRequested(prompt) => {
                auto_denied_approvals += 1;
                respond_to_approval(prompt.request.request_id).await?;
            }
            RuntimeEvent::TurnCompleted => {
                return Ok(TurnObservation {
                    latency: started.elapsed(),
                    ttft,
                    auto_denied_approvals,
                });
            }
            RuntimeEvent::Error(message) => {
                return Err(MoaError::ProviderError(message));
            }
            RuntimeEvent::AssistantStarted
            | RuntimeEvent::AssistantDelta(_)
            | RuntimeEvent::AssistantFinished { .. }
            | RuntimeEvent::ToolUpdate(_)
            | RuntimeEvent::UsageUpdated { .. }
            | RuntimeEvent::Notice(_) => {}
        }
    }
}
