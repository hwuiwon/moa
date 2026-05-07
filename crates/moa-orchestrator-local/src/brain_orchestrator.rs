//! `BrainOrchestrator` implementation for the local runtime.

use crate::*;

#[async_trait]
impl BrainOrchestrator for LocalOrchestrator {
    /// Starts a new session task and returns its handle.
    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle> {
        let initial_message = req.initial_message.clone();
        self.remember_detected_workspace_root(&req.workspace_id)
            .await;
        let session_id = SessionId::new();
        let now = Utc::now();
        let meta = SessionMeta {
            id: session_id,
            workspace_id: req.workspace_id.clone(),
            user_id: req.user_id.clone(),
            title: req.title.clone(),
            status: SessionStatus::Created,
            platform: req.platform.clone(),
            model: req.model.clone(),
            created_at: now,
            updated_at: now,
            parent_session_id: req.parent_session_id,
            ..SessionMeta::default()
        };
        self.session_store.create_session(meta).await?;
        append_event(
            &self.instrumented_session_store,
            &broadcast::channel(1).0,
            session_id,
            Event::SessionCreated {
                workspace_id: req.workspace_id.clone(),
                user_id: req.user_id.clone(),
                model: req.model.clone(),
            },
        )
        .await?;
        let bootstrap_report = self
            .maybe_bootstrap_workspace_memory(&req.workspace_id)
            .await;
        if let Some(message) = initial_message {
            append_event(
                &self.instrumented_session_store,
                &broadcast::channel(1).0,
                session_id,
                Event::UserMessage {
                    text: message.text,
                    attachments: message.attachments,
                },
            )
            .await?;
        }
        self.spawn_session(session_id, req.initial_message.is_some(), Vec::new())
            .await?;
        if let Some(report) = bootstrap_report {
            let sessions = self.sessions.read().await;
            if let Some(handle) = sessions.get(&session_id) {
                let source = report
                    .source_file
                    .as_deref()
                    .unwrap_or("workspace instructions");
                let total = report.ingest.inserted + report.ingest.superseded;
                let _ = handle.runtime_tx.send(RuntimeEvent::Notice(format!(
                    "Workspace graph memory initialized from {source} ({total} nodes written)."
                )));
            }
        }
        Ok(SessionHandle { session_id })
    }

    /// Resumes an existing persisted session by spawning a new background task if needed.
    async fn resume_session(&self, session_id: SessionId) -> Result<SessionHandle> {
        let session = self.session_store.get_session(session_id).await?;
        if self.handle_is_active(&session_id).await {
            if matches!(
                session.status,
                SessionStatus::Running | SessionStatus::WaitingApproval
            ) {
                return Ok(SessionHandle { session_id });
            }

            self.wait_for_handle_shutdown(&session_id).await;
            if self.handle_is_active(&session_id).await {
                return Ok(SessionHandle { session_id });
            }
        }

        let wake = self.session_store.wake(session_id).await?;
        self.remember_detected_workspace_root(&wake.session.workspace_id)
            .await;
        let initial_queued_messages = wake
            .pending_signals
            .into_iter()
            .map(BufferedUserMessage::from_pending_signal)
            .collect::<Result<Vec<_>>>()?;
        let initial_turn_requested =
            session_requires_processing(&wake.session, &wake.recent_events)
                || !initial_queued_messages.is_empty();
        self.spawn_session(session_id, initial_turn_requested, initial_queued_messages)
            .await?;
        Ok(SessionHandle { session_id })
    }

    /// Sends a signal to a running local session.
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        self.ensure_session_running(session_id).await?;
        if let SessionSignal::QueueMessage(message) = &signal {
            let pending = PendingSignal::queue_message(session_id, message.clone())?;
            self.session_store
                .store_pending_signal(session_id, pending)
                .await?;
        }
        let sessions = self.sessions.read().await;
        let handle = sessions
            .get(&session_id)
            .ok_or_else(|| MoaError::SessionNotFound(session_id))?;

        if matches!(signal, SessionSignal::HardCancel) {
            handle.cancel_token.cancel();
        }
        if matches!(signal, SessionSignal::HardCancel) {
            handle.hard_cancel_token.cancel();
        }

        handle
            .signal_tx
            .send(signal)
            .await
            .map_err(|_| MoaError::ProviderError("session signal channel closed".to_string()))
    }

    /// Lists persisted sessions matching the provided filter.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        self.session_store.list_sessions(filter).await
    }

    /// Returns buffered history plus live updates via local broadcast or Postgres LISTEN.
    async fn observe(&self, session_id: SessionId, _level: ObserveLevel) -> Result<EventStream> {
        let session = self.session_store.get_session(session_id).await?;
        let history = self
            .session_store
            .get_events(session_id, EventRange::all())
            .await?;
        let sessions = self.sessions.read().await;
        if let Some(handle) = sessions.get(&session_id)
            && !handle.finished.load(Ordering::SeqCst)
        {
            return Ok(EventStream::from_history_and_broadcast(
                session_id,
                history,
                handle.event_tx.subscribe(),
            ));
        }

        if matches!(
            session.status,
            SessionStatus::Running | SessionStatus::WaitingApproval
        ) {
            let next_seq = history
                .last()
                .map(|record| record.sequence_num.saturating_add(1))
                .unwrap_or(0);
            let live = SessionEventStream::subscribe(
                self.session_store.clone(),
                session_id,
                Some(next_seq),
            )
            .await?;
            return Ok(EventStream::from_history_and_channel(
                session_id,
                history,
                live.into_receiver(),
            ));
        }

        Ok(EventStream::from_events(history))
    }

    /// Subscribes to live runtime events. Returns `Ok(None)` when no
    /// actor is active; observation must not resume a dormant session
    /// (that would spawn a brain actor on every UI session switch).
    async fn observe_runtime(
        &self,
        session_id: SessionId,
    ) -> Result<Option<broadcast::Receiver<RuntimeEvent>>> {
        self.session_store.get_session(session_id).await?;
        let sessions = self.sessions.read().await;
        let Some(handle) = sessions.get(&session_id) else {
            return Ok(None);
        };
        if handle.finished.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(handle.runtime_tx.subscribe()))
    }

    /// Registers a local cron job backed by `tokio-cron-scheduler`.
    async fn schedule_cron(&self, spec: CronSpec) -> Result<CronHandle> {
        let job_name = spec.name.clone();
        let task_name = spec.task.clone();
        let job = Job::new_async(spec.schedule.as_str(), move |_id, _lock| {
            let job_name = job_name.clone();
            let task_name = task_name.clone();
            Box::pin(async move {
                tracing::info!(job = %job_name, task = %task_name, "running scheduled local job");
            })
        })
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let job_id = job.guid().to_string();
        self.scheduler
            .add(job)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(CronHandle::Local { id: job_id })
    }
}
