//! Remote load-test backend adapter.

use crate::*;
use moa_authz::{FgaClient, FgaConfig};
use moa_core::traits::{Identity, IdentityType};
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
    fga: FgaClient,
    identity: Identity,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
}

#[async_trait]
impl SessionTarget for RemoteTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        self.grant_workspace_member().await?;
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
        self.grant_session_participant(session_id).await?;
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

impl RemoteTarget {
    async fn grant_workspace_member(&self) -> Result<()> {
        self.grant_raw_tuple(
            format!("user:{}", self.identity.id),
            "member",
            format!("workspace:{}", self.workspace_id),
        )
        .await
    }

    async fn grant_session_participant(&self, session_id: SessionId) -> Result<()> {
        self.grant_raw_tuple(
            format!("user:{}", self.identity.id),
            "participant",
            format!("session:{session_id}"),
        )
        .await
    }

    async fn grant_raw_tuple(&self, user: String, relation: &str, object: String) -> Result<()> {
        self.fga
            .apply_raw(serde_json::json!({
                "authorization_model_id": self.fga.model_id(),
                "writes": {
                    "tuple_keys": [{
                        "user": user,
                        "relation": relation,
                        "object": object,
                    }],
                },
            }))
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("loadtest OpenFGA grant failed: {error}"))
            })
    }
}

pub(crate) async fn build_backend(
    options: &LoadTestOptions,
    config: &MoaConfig,
) -> Result<Arc<dyn SessionTarget>> {
    let identity = Identity {
        identity_type: IdentityType::User,
        id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let client = OrchestratorClient::new(&options.endpoint)
        .map_err(client_error)?
        .with_identity(identity.clone());
    let fga = live_fga_client()?;
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| config.model_for_task(ModelTask::MainLoop).to_string());
    Ok(Arc::new(RemoteTarget {
        client,
        fga,
        identity,
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

fn live_fga_client() -> Result<FgaClient> {
    FgaClient::new(FgaConfig {
        url: std::env::var("MOA_OPENFGA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10030".to_string()),
        preshared_key: std::env::var("MOA_OPENFGA_PRESHARED_KEY")
            .unwrap_or_else(|_| "localdev-preshared-key-do-not-use-in-prod".to_string()),
        store_id: std::env::var("MOA_OPENFGA_STORE_ID").map_err(|_| {
            MoaError::MissingEnvironmentVariable("MOA_OPENFGA_STORE_ID".to_string())
        })?,
        model_id: std::env::var("MOA_OPENFGA_MODEL_ID").map_err(|_| {
            MoaError::MissingEnvironmentVariable("MOA_OPENFGA_MODEL_ID".to_string())
        })?,
        timeout_ms: 5_000,
    })
    .map_err(|error| MoaError::ProviderError(format!("OpenFGA client config: {error}")))
}
