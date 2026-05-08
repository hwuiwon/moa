//! Remote load-test backend adapter.

use crate::*;
use moa_core::wire::StartTurnRequest;
use moa_orchestrator_client::OrchestratorClient;

const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[async_trait]
pub(crate) trait SessionTarget: Send + Sync {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId>;
    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation>;
    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta>;
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;
    async fn cleanup(&self) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct RemoteTarget {
    client: OrchestratorClient,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
}

#[async_trait]
impl SessionTarget for RemoteTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        let session_id = SessionId::new();
        let now = chrono::Utc::now();
        let meta = SessionMeta {
            id: session_id,
            workspace_id: self.workspace_id.clone(),
            user_id: self.user_id.clone(),
            title: Some(plan.title.clone()),
            status: SessionStatus::Created,
            platform: Platform::Cli,
            platform_channel: None,
            model: self.model.clone(),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
            total_input_tokens: 0,
            total_input_tokens_uncached: 0,
            total_input_tokens_cache_write: 0,
            total_input_tokens_cache_read: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        };

        self.client
            .create_session(meta.clone())
            .await
            .map_err(client_error)?;
        self.client
            .append_event(
                session_id,
                Event::SessionCreated {
                    workspace_id: self.workspace_id.clone(),
                    user_id: self.user_id.clone(),
                    model: self.model.clone(),
                },
            )
            .await
            .map_err(client_error)?;
        self.client
            .init_session_vo(session_id, meta)
            .await
            .map_err(client_error)?;

        Ok(session_id)
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation> {
        let started = Instant::now();
        let handle = self.client.session(session_id.to_string());
        let response = handle
            .start_turn(
                StartTurnRequest {
                    user_message: prompt.to_string(),
                    attachments: Vec::new(),
                    model: Some(self.model.to_string()),
                },
                Some(&format!("loadtest-turn-{session_id}-{}", Uuid::now_v7())),
            )
            .await
            .map_err(client_error)?;
        let turn_id = response.turn_id.ok_or_else(|| {
            MoaError::ProviderError(format!(
                "loadtest turn for session {session_id} queued unexpectedly"
            ))
        })?;
        let outcome = handle
            .await_turn_outcome(&turn_id, timeout, SNAPSHOT_POLL_INTERVAL)
            .await
            .map_err(client_error)?;

        match outcome.kind {
            moa_core::wire::TurnOutcomeKind::Completed => Ok(TurnObservation {
                latency: started.elapsed(),
                ttft: None,
                auto_denied_approvals: 0,
            }),
            moa_core::wire::TurnOutcomeKind::Cancelled => Err(MoaError::Cancelled),
            moa_core::wire::TurnOutcomeKind::Failed => {
                Err(MoaError::ProviderError(outcome.message))
            }
        }
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.client
            .get_session(session_id)
            .await
            .map_err(client_error)
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.client
            .get_events(session_id, moa_core::EventRange::all())
            .await
            .map_err(client_error)
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
}

pub(crate) async fn build_backend(
    options: &LoadTestOptions,
    config: &MoaConfig,
) -> Result<Arc<dyn SessionTarget>> {
    let client = OrchestratorClient::new(&options.endpoint).map_err(client_error)?;
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| config.model_for_task(ModelTask::MainLoop).to_string());
    Ok(Arc::new(RemoteTarget {
        client,
        workspace_id: WorkspaceId::new(format!(
            "loadtest-{}",
            &Uuid::now_v7().simple().to_string()[..8]
        )),
        user_id: UserId::new("loadtest"),
        model: ModelId::new(model),
    }))
}

fn client_error(error: moa_orchestrator_client::Error) -> MoaError {
    MoaError::ProviderError(format!("orchestrator client error: {error}"))
}
